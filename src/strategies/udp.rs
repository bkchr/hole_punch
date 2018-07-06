use authenticator::Authenticator;
use error::*;
use strategies::{
    Connection, ConnectionId, GetConnectionId, LocalAddressInformation, NewConnection,
    NewConnectionFuture, NewConnectionHandle, NewStream, NewStreamFuture, NewStreamHandle,
    PeerAddressInformation, Strategy, Stream,
};
use timeout::Timeout;
use Config;

use std::{
    cmp::{min, Ordering},
    collections::{
        hash_map::Entry::{Occupied, Vacant}, BinaryHeap, HashMap,
    }, fmt,
    io::{self, Cursor}, mem::size_of, net::SocketAddr, time::Duration,
};

use futures::{
    self, stream::Fuse,
    sync::mpsc::{channel, unbounded, Receiver, Sender, UnboundedReceiver, UnboundedSender},
    sync::oneshot, Async::{NotReady, Ready}, AsyncSink, Future, IntoFuture, Poll, Sink, StartSend,
    Stream as FStream,
};

use tokio_io::{AsyncRead, AsyncWrite};

use tokio_core::{net::UdpSocket, reactor::Handle};

use bytes::{Buf, BufMut, BytesMut};

mod udp {
    use super::*;

    /// Represents an incoming connection
    struct ConnectionWrapper {
        /// This sender is used to forward data to the stream object
        sender: Sender<BytesMut>,
        /// This receiver is used to forward data from the stream object
        recv: Receiver<BytesMut>,
    }

    impl ConnectionWrapper {
        /// Creates a new `StreamWrapper`
        ///
        /// * `recv` - The receiver to receive data from the connected `UdpServerStream`
        /// * `sender` - The sender to send data to the connected `UdpServerStream`
        fn new(mut recv: Receiver<BytesMut>, sender: Sender<BytesMut>) -> ConnectionWrapper {
            // we need to poll the receiver once, so that *this* `Task` is registered to be woken up,
            // when someone wants to send data
            let _ = recv.poll();
            ConnectionWrapper { sender, recv }
        }

        /// Forwards received data to the connected `UdpServerStream`
        fn recv(&mut self, data: BytesMut) {
            // if we see an error here, abort the function.
            // this connection will be dropped in the next `poll` call of `UdpServer`.
            // the check is only required for start_send, but to mute the warning, check the result of
            // poll_complete, too.
            if self.sender.start_send(data).is_err() || self.sender.poll_complete().is_err() {
                eprintln!("ERROR");
                return;
            }
        }

        /// Checks if the connected `UdpServerStream` wants to send data
        ///
        /// # Return value
        ///
        /// * `Ok(Some(d))` - The stream wants to send data
        /// * `Ok(None)` - The stream does not want to send data
        /// * `Err(_)` - The stream was dropped and this connection can also be dropped
        fn send(&mut self) -> Result<Option<BytesMut>> {
            match self.recv.poll() {
                Ok(Ready(Some(data))) => Ok(Some(data)),
                Ok(NotReady) => Ok(None),
                // Err(_) | Ok(Ready(None)), we interpret both as that the stream was dropped
                _ => bail!("stream dropped"),
            }
        }
    }

    pub struct UdpServer {
        new_connection: Receiver<(UdpServerStream, SocketAddr)>,
        addr: SocketAddr,
    }

    impl UdpServer {
        pub fn new(
            socket: UdpSocket,
            channel_buffer: usize,
            handle: &Handle,
        ) -> (UdpServer, Connect) {
            let addr = socket.local_addr();
            let (inner, new_connection, connect) = UdpServerInner::new(socket, channel_buffer);

            handle.spawn(inner.map_err(|e| eprintln!("Inner error: {:?}", e)));

            (
                UdpServer {
                    new_connection,
                    addr: addr.unwrap(),
                },
                connect,
            )
        }

        pub fn local_addr(&self) -> Result<SocketAddr> {
            Ok(self.addr)
        }
    }

    impl FStream for UdpServer {
        type Item = (UdpServerStream, SocketAddr);
        type Error = io::Error;

        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            self.new_connection
                .poll()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "UdpServer Stream poll failed"))
        }
    }

    /// The `UdpServer` checks the socket for incoming data, routes the data to the appropriate
    /// `StreamWrapper` and sends data from `StreamWrapper` over the socket.
    struct UdpServerInner {
        /// The socket the server is listening on
        socket: UdpSocket,
        /// All active connections we the server is handling
        connections: HashMap<SocketAddr, ConnectionWrapper>,
        /// Temp buffer for receiving messages
        buf: Vec<u8>,
        /// The buffer size of the `StreamWrapper` and `UdpServerStream` channels
        buffer_size: usize,
        /// Overflow element when the Socket currently is not able to send data
        send_overflow: Option<(BytesMut, SocketAddr)>,
        new_connection: Sender<(UdpServerStream, SocketAddr)>,
        connect_to: Fuse<UnboundedReceiver<(SocketAddr, oneshot::Sender<UdpServerStream>)>>,
        next_con_id: ConnectionId,
    }

    impl UdpServerInner {
        /// Creates a new instance of the `UdpServer`
        ///
        /// * `socket` - The `UdpSocket` this server should use.
        /// * `channel_buffer` - Defines the buffer size of the channel that connects
        ///                      `StreamWrapper` and `UdpServerStream`. Both sides drop data/return
        ///                      `WouldBlock` if the channel is full.
        fn new(
            socket: UdpSocket,
            channel_buffer: usize,
        ) -> (
            UdpServerInner,
            Receiver<(UdpServerStream, SocketAddr)>,
            Connect,
        ) {
            let (ncsender, ncreceiver) = channel(channel_buffer);
            let (csender, creceiver) = unbounded();

            (
                UdpServerInner {
                    socket,
                    buf: vec![0; 1400],
                    connections: HashMap::new(),
                    buffer_size: channel_buffer,
                    send_overflow: None,
                    new_connection: ncsender,
                    connect_to: creceiver.fuse(),
                    next_con_id: 0,
                },
                ncreceiver,
                Connect {
                    connect_sender: csender,
                },
            )
        }

        /// Checks all connections if they want to send data.
        /// While checking for data to send, connections that return a `Err(_)` are dropped from the
        /// hash map.
        fn send_data(&mut self) {
            // the borrow checker does not want 2 mutable references to self, but with this trick it
            // works
            fn retain(
                connections: &mut HashMap<SocketAddr, ConnectionWrapper>,
                socket: &mut UdpSocket,
            ) -> Option<(BytesMut, SocketAddr)> {
                let mut overflow = None;
                connections.retain(|addr, c| loop {
                    if overflow.is_some() {
                        return true;
                    }

                    let _ = match c.send() {
                        Ok(Some(data)) => if let Ready(()) = socket.poll_write() {
                            socket.send_to(&data, &addr)
                        } else {
                            overflow = Some((data, addr.clone()));
                            return true;
                        },
                        Ok(None) => return true,
                        _ => return false,
                    };
                });

                overflow
            }

            if let (Ready(()), Some((data, addr))) =
                (self.socket.poll_write(), self.send_overflow.take())
            {
                let _ = self.socket.send_to(&data, &addr);
            }

            if let Ready(()) = self.socket.poll_write() {
                self.send_overflow = retain(&mut self.connections, &mut self.socket);
            }
        }

        /// Creates a new `StreamWrapper` and the connected `UdpServerStream`
        fn create_connection_and_stream(
            buffer_size: usize,
            local_addr: SocketAddr,
            remote_addr: SocketAddr,
            id: ConnectionId,
        ) -> (ConnectionWrapper, UdpServerStream) {
            let (con_sender, con_receiver) = channel(buffer_size);
            let (stream_sender, stream_receiver) = channel(buffer_size);

            (
                ConnectionWrapper::new(stream_receiver, con_sender),
                UdpServerStream::new(con_receiver, stream_sender, local_addr, remote_addr, id),
            )
        }

        fn connect(&mut self, addr: SocketAddr) -> UdpServerStream {
            let id = self.next_con_id;
            self.next_con_id += 1;
            let (con, stream) = Self::create_connection_and_stream(
                self.buffer_size,
                self.socket.local_addr().unwrap(),
                addr,
                id,
            );
            self.connections.insert(addr, con);
            stream
        }

        pub fn local_addr(&self) -> Result<SocketAddr> {
            self.socket.local_addr().map_err(|e| e.into())
        }
    }

    impl Future for UdpServerInner {
        type Item = ();
        type Error = io::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.send_data();

            loop {
                match self.connect_to.poll() {
                    Ok(Ready(Some((addr, sender)))) => {
                        let stream = self.connect(addr);
                        let _ = sender.send(stream);
                    }
                    _ => break,
                }
            }

            loop {
                let (len, addr) = try_nb!(self.socket.recv_from(self.buf.as_mut()));

                // check if the address is already in our connections map
                match self.connections.entry(addr) {
                    Occupied(mut entry) => entry.get_mut().recv(BytesMut::from(&self.buf[..len])),
                    Vacant(entry) => {
                        let id = self.next_con_id;
                        self.next_con_id += 1;
                        let (mut con, stream) = Self::create_connection_and_stream(
                            self.buffer_size,
                            self.socket.local_addr().unwrap(),
                            addr,
                            id,
                        );
                        entry.insert(con).recv(BytesMut::from(&self.buf[..len]));

                        let _ = self.new_connection.start_send((stream, addr));
                        let _ = self.new_connection.poll_complete();
                    }
                };
            }
        }
    }

    /// UdpStream that is created by a `UdpServer` and is connected to a `StreamWrapper`.
    pub struct UdpServerStream {
        /// The sender to send data to the connected `StreamWrapper` and effectively over the socket
        sender: Sender<BytesMut>,
        /// The receiver to recv data from the connected `StreamWrapper`
        receiver: Receiver<BytesMut>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        id: ConnectionId,
    }

    impl UdpServerStream {
        /// Creates a new UdpServerStream
        fn new(
            receiver: Receiver<BytesMut>,
            sender: Sender<BytesMut>,
            local_addr: SocketAddr,
            remote_addr: SocketAddr,
            id: ConnectionId,
        ) -> UdpServerStream {
            UdpServerStream {
                receiver,
                sender,
                local_addr,
                remote_addr,
                id,
            }
        }

        pub fn local_addr(&self) -> SocketAddr {
            self.local_addr
        }
        pub fn remote_addr(&self) -> SocketAddr {
            self.remote_addr
        }

        pub fn get_id(&self) -> ConnectionId {
            self.id
        }
    }

    impl FStream for UdpServerStream {
        type Item = BytesMut;
        type Error = ();

        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            self.receiver.poll()
        }
    }

    impl Sink for UdpServerStream {
        type SinkItem = BytesMut;
        type SinkError = <Sender<BytesMut> as Sink>::SinkError;

        fn start_send(
            &mut self,
            item: Self::SinkItem,
        ) -> StartSend<Self::SinkItem, Self::SinkError> {
            self.sender.start_send(item)
        }

        fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
            self.sender.poll_complete()
        }
    }

    fn to_io_error<E: fmt::Debug>(error: E) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{:?}", error))
    }

    impl io::Write for UdpServerStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let AsyncSink::NotReady(_) =
                self.start_send(BytesMut::from(buf)).map_err(to_io_error)?
            {
                return Err(io::ErrorKind::WouldBlock.into());
            }

            self.poll_complete().map_err(to_io_error)?;

            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl io::Read for UdpServerStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let res = self.poll().map_err(to_io_error)?;

            match res {
                NotReady => Err(io::ErrorKind::WouldBlock.into()),
                Ready(Some(data)) => {
                    // If buf is too small, elements will get lost
                    // TODO: maybe integrate tmp buffer for 'lost' elements.
                    let len = min(buf.len(), data.len());
                    &buf[..len].copy_from_slice(&data.as_ref()[..len]);
                    if buf.len() < data.len() {
                        eprintln!("DATALOSS!!");
                    }
                    Ok(len)
                }
                Ready(None) => Ok(0),
            }
        }
    }

    impl AsyncRead for UdpServerStream {}

    impl AsyncWrite for UdpServerStream {
        fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
            Ok(Ready(()))
        }
    }

    #[derive(Clone)]
    pub struct Connect {
        connect_sender: UnboundedSender<(SocketAddr, oneshot::Sender<UdpServerStream>)>,
    }

    impl Connect {
        pub fn connect(&mut self, addr: SocketAddr) -> Result<WaitForConnect> {
            let (sender, recv) = oneshot::channel();

            self.connect_sender
                .unbounded_send((addr, sender))
                .map(|_| WaitForConnect(recv))
                .map_err(|_| "error sending udp connect request".into())
        }
    }

    pub struct WaitForConnect(oneshot::Receiver<UdpServerStream>);

    impl Future for WaitForConnect {
        type Item = UdpServerStream;
        type Error = Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.0.poll().map_err(|e| e.into())
        }
    }

    /// The type of a stream the `ConnectUdpServer` provides.
    pub enum StreamType {
        /// The stream was created by connecting to a remote address.
        Connect,
        /// The stream was created by accepting a connection from a remote address.
        Accept,
    }

}

pub struct StrategyWrapper {
    server: udp::UdpServer,
    handle: Handle,
    new_connection_handle: NewConnectionHandleWrapper,
}

impl StrategyWrapper {
    fn new(config: &Config, handle: Handle, _: Authenticator) -> Result<Self> {
        let socket = UdpSocket::bind(&config.shitty_udp_listen_address, &handle)?;
        let (server, connect) = udp::UdpServer::new(socket, 10, &handle);
        let new_connection_handle = NewConnectionHandleWrapper::new(connect, handle.clone());
        Ok(StrategyWrapper {
            server,
            handle,
            new_connection_handle,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }
}

fn spawn_reliable_con(
    con: udp::UdpServerStream,
    remote_addr: SocketAddr,
    handle: &Handle,
) -> Connection {
    let local_addr = con.local_addr();
    let (rel_con, con) = ReliableConnection::new(con, handle, local_addr, remote_addr);

    handle.spawn(rel_con);

    Connection::new(con)
}

impl FStream for StrategyWrapper {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.server
            .poll()
            .map(|r| r.map(|o| o.map(|con| spawn_reliable_con(con.0, con.1, &self.handle))))
            .map_err(|e| e.into())
    }
}

impl NewConnection for StrategyWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        self.new_connection_handle.new_connection(addr)
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(self.new_connection_handle.clone())
    }
}

impl LocalAddressInformation for StrategyWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.server.local_addr().unwrap()
    }
}

#[derive(Clone)]
struct NewConnectionHandleWrapper {
    new_con: udp::Connect,
    handle: Handle,
}

impl NewConnectionHandleWrapper {
    fn new(new_con: udp::Connect, handle: Handle) -> NewConnectionHandleWrapper {
        NewConnectionHandleWrapper { new_con, handle }
    }
}

impl NewConnection for NewConnectionHandleWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        let handle = self.handle.clone();
        NewConnectionFuture::new(
            self.new_con
                .connect(addr)
                .into_future()
                .flatten()
                .map(move |v| {
                    let con = spawn_reliable_con(v, addr, &handle);
                    Connection::new(con)
                })
                .map_err(|e| e.into()),
        )
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(self.clone())
    }
}

struct ConnectionWrapper {
    new_stream_recv: UnboundedReceiver<StreamWrapper>,
    id: ConnectionId,
    create_new_stream: NewStreamHandle,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl ConnectionWrapper {
    fn new(
        new_stream_recv: UnboundedReceiver<StreamWrapper>,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
        id: ConnectionId,
        create_new_stream: NewStreamHandle,
    ) -> ConnectionWrapper {
        ConnectionWrapper {
            new_stream_recv,
            id,
            create_new_stream,
            peer_addr,
            local_addr,
        }
    }
}

impl FStream for ConnectionWrapper {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.new_stream_recv
            .poll()
            .map(|r| r.map(|o| o.map(|v| Stream::new(v))))
            .map_err(|_| Error::from("ConnectionWrapper::poll() - unknown error"))
    }
}

impl NewStream for ConnectionWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        self.create_new_stream.new_stream()
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.create_new_stream.clone()
    }
}

impl LocalAddressInformation for ConnectionWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl PeerAddressInformation for ConnectionWrapper {
    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
}

impl GetConnectionId for ConnectionWrapper {
    fn connection_id(&self) -> ConnectionId {
        self.id
    }
}

enum ReliableMsgType {
    Ack = 0,
    Msg = 1,
}

impl ReliableMsgType {
    fn try_from(data: u8) -> Result<ReliableMsgType> {
        match data {
            0 => Ok(ReliableMsgType::Ack),
            1 => Ok(ReliableMsgType::Msg),
            v => bail!("unknown reliable msg type for: {}", v),
        }
    }
}

struct SortedRecv {
    id: u64,
    data: BytesMut,
}

impl PartialEq for SortedRecv {
    fn eq(&self, other: &SortedRecv) -> bool {
        self.id == other.id
    }
}

impl Eq for SortedRecv {}

impl PartialOrd for SortedRecv {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        other.id.partial_cmp(&self.id)
    }
}

impl Ord for SortedRecv {
    fn cmp(&self, other: &SortedRecv) -> Ordering {
        let ord = self.partial_cmp(other).unwrap();

        // BinaryHeap is max heap, but we require min heap
        match ord {
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
            Ordering::Equal => ord,
        }
    }
}

fn get_reliable_msg_header_len() -> usize {
    // length of the entire package, version as u8, id as u64, session id as u8, ty as u8
    size_of::<u64>() + size_of::<u8>() + size_of::<u64>() + size_of::<u16>() + size_of::<u8>()
}

fn pack_reliable_msg(id: u64, stream_id: u16, ty: ReliableMsgType, data: &[u8]) -> BytesMut {
    let len = get_reliable_msg_header_len() + data.len();
    let mut bytes = BytesMut::with_capacity(get_reliable_msg_header_len() + data.len());

    bytes.put_u64_be(len as u64);
    bytes.put_u8(0);
    bytes.put_u64_be(id);
    bytes.put_u16_be(stream_id);
    bytes.put_u8(ty as u8);
    bytes.put_slice(data);

    bytes
}

fn unpack_reliable_msg(
    mut data: BytesMut,
) -> Result<(u64, u16, ReliableMsgType, Option<BytesMut>)> {
    let len = data.len();

    if len < get_reliable_msg_header_len() {
        bail!(
            "received data does not contain enough data to hold the header(length: {})",
            len
        );
    }

    let (data_len, id, stream_id, ty) = {
        let mut src = Cursor::new(&data);

        let recv_len = src.get_u64_be();

        // TODO, we need to support MTU resize
        if recv_len > len as u64 {
            bail!(
                "recv_len({}) is greater than the length of the given buffer({})!",
                recv_len,
                len
            );
        }

        if recv_len == 0 {
            bail!("recv_len is 0!");
        }

        // version
        src.get_u8();

        let id = src.get_u64_be();

        let stream_id = src.get_u16_be();

        let ty = ReliableMsgType::try_from(src.get_u8())?;

        (
            recv_len as usize - get_reliable_msg_header_len(),
            id,
            stream_id,
            ty,
        )
    };

    let data = if data_len > 0 {
        data.advance(get_reliable_msg_header_len());
        Some(data)
    } else {
        None
    };

    Ok((id, stream_id, ty, data))
}

struct ReliableStream {
    sender: UnboundedSender<BytesMut>,
    recv: UnboundedReceiver<BytesMut>,
    next_id: u64,
    stream_id: u16,
    received_data: BinaryHeap<SortedRecv>,
    // the last id that was propagated
    last_propagated_id: u64,
    send_msgs_without_ack: HashMap<u64, (BytesMut, Timeout)>,
}

impl ReliableStream {
    fn new(
        stream_id: u16,
        new_stream_handle: NewStreamHandle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        con_id: ConnectionId,
    ) -> (ReliableStream, StreamWrapper) {
        let (sender, poll_recv) = unbounded();
        let (sink_sender, mut recv) = unbounded();

        // We need to poll the receiver onces, that the *current* task is registered to be woken up,
        // when new data should be send!
        let _ = recv.poll();

        (
            ReliableStream {
                recv,
                sender,
                next_id: 1,
                stream_id,
                received_data: BinaryHeap::new(),
                last_propagated_id: 0,
                send_msgs_without_ack: HashMap::new(),
            },
            StreamWrapper::new(
                poll_recv,
                sink_sender,
                new_stream_handle,
                local_addr,
                remote_addr,
                con_id,
            ),
        )
    }

    fn recv(&mut self, id: u64, data: BytesMut) -> Result<()> {
        if self.last_propagated_id + 1 == id {
            self.last_propagated_id = id;

            self.sender
                .unbounded_send(data)
                .map_err(|_| "`ReliableStream::recv` send error")?;
            self.check_received_data()
        } else {
            self.received_data.push(SortedRecv { id, data });

            Ok(())
        }
    }

    fn check_received_data(&mut self) -> Result<()> {
        loop {
            if Some(self.last_propagated_id + 1) == self.received_data.peek().map(|m| m.id) {
                let msg = self.received_data.pop().unwrap();
                self.last_propagated_id = msg.id;

                self.sender
                    .unbounded_send(msg.data)
                    .map_err(|_| "error sending")?;
            } else {
                return Ok(());
            }
        }
    }

    fn send(&mut self, handle: &Handle) -> Poll<Option<BytesMut>, Error> {
        let data = match try_ready!(
            self.recv
                .poll()
                .map_err(|_| "`ReliableStream::send` poll error")
        ) {
            Some(data) => data,
            None => bail!("None received in `ReliableStream::send`"),
        };

        let id = self.next_id;
        self.next_id += 1;

        let data = pack_reliable_msg(id, self.stream_id, ReliableMsgType::Msg, &data);
        self.send_msgs_without_ack.insert(
            id,
            (
                data.clone(),
                Timeout::new(Duration::from_millis(200), handle),
            ),
        );

        Ok(Ready(Some(data)))
    }

    fn ack_recv(&mut self, id: u64) {
        self.send_msgs_without_ack.remove(&id);
    }

    fn check_resend(&mut self) -> Option<Vec<BytesMut>> {
        if self.send_msgs_without_ack.is_empty() {
            return None;
        }

        let mut result = Vec::new();

        for &mut (ref data, ref mut timeout) in self.send_msgs_without_ack.values_mut() {
            if timeout.poll().is_err() {
                timeout.reset();
                let _ = timeout.poll();

                result.push(data.clone());
            }
        }

        Some(result)
    }
}

pub struct ReliableConnection {
    con: udp::UdpServerStream,
    handle: Handle,
    next_stream_id: u16,
    streams: HashMap<u16, ReliableStream>,
    new_stream_recv: UnboundedReceiver<oneshot::Sender<StreamWrapper>>,
    new_stream_handle: NewStreamHandle,
    new_stream_inform: UnboundedSender<StreamWrapper>,
    known_streams: Vec<u16>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    id: ConnectionId,
}

impl ReliableConnection {
    fn new(
        con: udp::UdpServerStream,
        handle: &Handle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (ReliableConnection, ConnectionWrapper) {
        let (new_stream_handle, new_stream_recv) = unbounded();
        let (new_stream_inform, new_incoming_stream_recv) = unbounded();
        let id = con.get_id();

        let new_stream_handle =
            NewStreamHandle::new(NewStreamHandleWrapper::new(new_stream_handle));

        let con = ReliableConnection {
            con,
            handle: handle.clone(),
            next_stream_id: 0,
            streams: HashMap::new(),
            new_stream_recv,
            new_stream_handle: new_stream_handle.clone(),
            new_stream_inform,
            known_streams: Vec::new(),
            local_addr,
            remote_addr,
            id,
        };

        let udp_con = ConnectionWrapper::new(
            new_incoming_stream_recv,
            local_addr,
            remote_addr,
            id,
            new_stream_handle,
        );

        (con, udp_con)
    }

    fn send_ack(&mut self, id: u64, session: u16) {
        let _ = self
            .con
            .start_send(pack_reliable_msg(id, session, ReliableMsgType::Ack, &[]));
        let _ = self.con.poll_complete();
    }

    fn create_new_stream(&mut self) -> StreamWrapper {
        let (session, con) = ReliableStream::new(
            self.next_stream_id,
            self.new_stream_handle.clone(),
            self.local_addr,
            self.remote_addr,
            self.id,
        );

        self.streams.insert(self.next_stream_id, session);
        self.known_streams.push(self.next_stream_id);
        self.next_stream_id += 1;

        con
    }

    fn check_for_new_streams(&mut self) {
        loop {
            let new = match self.new_stream_recv.poll() {
                Ok(Ready(Some(new))) => new,
                _ => return,
            };

            let con = self.create_new_stream();
            let _ = new.send(con);
        }
    }

    fn check_send_data(&mut self) {
        fn retain(
            streams: &mut HashMap<u16, ReliableStream>,
            stream: &mut udp::UdpServerStream,
            handle: &Handle,
        ) {
            streams.retain(|_, s| {
                if let Some(resend) = s.check_resend() {
                    resend.into_iter().for_each(|v| {
                        let _ = stream.start_send(v);
                    });
                    let _ = stream.poll_complete();
                }

                loop {
                    match s.send(handle) {
                        Ok(Ready(None)) | Ok(NotReady) => return true,
                        Ok(Ready(Some(data))) => {
                            let _ = stream.start_send(data);
                            let _ = stream.poll_complete();
                        }
                        Err(e) => {
                            eprintln!("{:?}", e);
                            return false;
                        }
                    }
                }
            })
        }

        retain(&mut self.streams, &mut self.con, &self.handle)
    }

    fn check_recv_data(&mut self) -> Poll<(), ()> {
        loop {
            let msg = match try_ready!(self.con.poll().map_err(|_| ())) {
                Some(msg) => msg,
                None => {
                    eprintln!("ReliableConnection closed!");
                    return Err(());
                }
            };

            let (id, stream_id, ty, data) = match unpack_reliable_msg(msg) {
                Err(e) => {
                    eprintln!("Error: {:?}", e);
                    continue;
                }
                Ok(msg) => msg,
            };

            match (ty, data) {
                (ReliableMsgType::Ack, None) => {
                    self.streams.get_mut(&stream_id).map(|s| s.ack_recv(id));
                }
                (ReliableMsgType::Msg, Some(data)) => {
                    self.send_ack(id, stream_id);

                    match self.streams.entry(stream_id) {
                        Occupied(mut session) => {
                            if session.get_mut().recv(id, data).is_err() {
                                session.remove_entry();
                            }
                        }
                        Vacant(entry) => {
                            if !self.known_streams.contains(&stream_id) {
                                eprintln!("New stream found: {}", stream_id);
                                let (mut session, con) = ReliableStream::new(
                                    stream_id,
                                    self.new_stream_handle.clone(),
                                    self.local_addr,
                                    self.remote_addr,
                                    self.id,
                                );

                                let _ = session.recv(id, data);

                                entry.insert(session);
                                self.known_streams.push(stream_id);
                                self.next_stream_id = match stream_id.checked_add(1) {
                                    Some(val) => val,
                                    None => {
                                        eprintln!("stream_id out of bounds");
                                        return Err(());
                                    }
                                };

                                let _ = self.new_stream_inform.unbounded_send(con);
                            } else {
                                eprintln!("received package for stream({}), after the stream was finished!", stream_id);
                            }
                        }
                    }
                }
                _ => {
                    eprintln!("received weird shit and will ignore it!");
                }
            }
        }
    }
}

impl Future for ReliableConnection {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.check_for_new_streams();
        self.check_send_data();
        self.check_recv_data()?;

        if self.streams.is_empty() {
            eprintln!("Goodbye: {} -> {}", self.local_addr, self.remote_addr);
            // we are done
            Ok(Ready(()))
        } else {
            Ok(NotReady)
        }
    }
}

pub struct StreamWrapper {
    recv: UnboundedReceiver<BytesMut>,
    sender: UnboundedSender<BytesMut>,
    new_stream_handle: NewStreamHandle,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    con_id: ConnectionId,
}

impl StreamWrapper {
    fn new(
        recv: UnboundedReceiver<BytesMut>,
        sender: UnboundedSender<BytesMut>,
        new_stream_handle: NewStreamHandle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        con_id: ConnectionId,
    ) -> StreamWrapper {
        StreamWrapper {
            recv,
            sender,
            new_stream_handle,
            local_addr,
            remote_addr,
            con_id,
        }
    }
}

impl FStream for StreamWrapper {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.recv
            .poll()
            .map_err(|_| "error `StreamWrapper::poll`".into())
    }
}

impl Sink for StreamWrapper {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.sender
            .start_send(item)
            .map_err(|_| "error `StreamWrapper::start_send`".into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sender
            .poll_complete()
            .map_err(|_| "error `StreamWrapper::poll_complete`".into())
    }
}

impl LocalAddressInformation for StreamWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl PeerAddressInformation for StreamWrapper {
    fn peer_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl GetConnectionId for StreamWrapper {
    fn connection_id(&self) -> ConnectionId {
        self.con_id
    }
}

impl NewStream for StreamWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        self.new_stream_handle.new_stream()
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.new_stream_handle.clone()
    }
}

#[derive(Clone)]
struct NewStreamHandleWrapper {
    new_stream_handle: UnboundedSender<oneshot::Sender<StreamWrapper>>,
}

impl NewStreamHandleWrapper {
    fn new(
        new_stream_handle: UnboundedSender<oneshot::Sender<StreamWrapper>>,
    ) -> NewStreamHandleWrapper {
        NewStreamHandleWrapper { new_stream_handle }
    }
}

impl NewStream for NewStreamHandleWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let (sender, recv) = oneshot::channel();
        let _ = self.new_stream_handle.unbounded_send(sender);

        NewStreamFuture::new(recv.map(|v| Stream::new(v)).map_err(|e| e.into()))
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(self.clone())
    }
}

pub fn init(handle: Handle, config: &Config, authenticator: Authenticator) -> Result<Strategy> {
    Ok(Strategy::new(StrategyWrapper::new(
        config,
        handle,
        authenticator,
    )?))
}
