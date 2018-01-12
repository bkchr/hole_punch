use errors::*;

use std::{fmt, io};
use std::net::SocketAddr;
use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cmp::min;
use std::result;

use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use futures::Async::{NotReady, Ready};
use futures::future::FutureResult;
use futures::{AsyncSink, StartSend};
use futures::sync::mpsc::{channel, unbounded, Receiver, SendError, Sender, UnboundedReceiver,
                          UnboundedSender};
use futures::sync::oneshot;
use futures::{self, Future, IntoFuture, Poll, Sink, Stream};
use futures::stream::Fuse;
use futures::task;

use bytes::{Bytes, BytesMut};

/// Represents an `UdpStream` that is connected to a remote socket.
pub struct UdpConnectStream {
    /// The underlying socket
    socket: UdpSocket,
}

impl UdpConnectStream {
    pub fn new(socket: UdpSocket) -> UdpConnectStream {
        UdpConnectStream { socket }
    }

    pub fn port(&self) -> Result<u16> {
        self.socket
            .local_addr()
            .map(|a| a.port())
            .chain_err(|| "error")
    }
}

impl io::Write for UdpConnectStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for UdpConnectStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.recv(buf)
    }
}

impl AsyncRead for UdpConnectStream {}

impl AsyncWrite for UdpConnectStream {
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}

/// Represents an incoming connection
struct UdpConnection {
    /// This sender is used to forward data to the stream object
    sender: Sender<Bytes>,
    /// This receiver is used to forward data from the stream object
    recv: Receiver<Bytes>,
}

impl UdpConnection {
    /// Creates a new `UdpConnection`
    ///
    /// * `recv` - The receiver to receive data from the connected `UdpServerStream`
    /// * `sender` - The sender to send data to the connected `UdpServerStream`
    fn new(mut recv: Receiver<Bytes>, sender: Sender<Bytes>) -> UdpConnection {
        // we need to poll the receiver once, so that *this* `Task` is registered to be woken up,
        // when someone wants to send data
        let _ = recv.poll();
        UdpConnection { sender, recv }
    }

    /// Forwards received data to the connected `UdpServerStream`
    fn recv(&mut self, data: Bytes) {
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
    fn send(&mut self) -> Result<Option<Bytes>> {
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
    fn new(socket: UdpSocket, channel_buffer: usize, handle: &Handle) -> (UdpServer, Connect) {
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

impl Stream for UdpServer {
    type Item = (UdpServerStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.new_connection
            .poll()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "UdpServer Stream poll failed"))
    }
}

/// The `UdpServer` checks the socket for incoming data, routes the data to the appropriate
/// `UdpConnection` and sends data from `UdpConnection` over the socket.
struct UdpServerInner {
    /// The socket the server is listening on
    socket: UdpSocket,
    /// All active connections we the server is handling
    connections: HashMap<SocketAddr, UdpConnection>,
    /// Temp buffer for receiving messages
    buf: Vec<u8>,
    /// The buffer size of the `UdpConnection` and `UdpServerStream` channels
    buffer_size: usize,
    /// Overflow element when the Socket currently is not able to send data
    send_overflow: Option<(Bytes, SocketAddr)>,
    new_connection: Sender<(UdpServerStream, SocketAddr)>,
    connect_to: Fuse<UnboundedReceiver<(SocketAddr, oneshot::Sender<UdpServerStream>)>>,
}

impl UdpServerInner {
    /// Creates a new instance of the `UdpServer`
    ///
    /// * `socket` - The `UdpSocket` this server should use.
    /// * `channel_buffer` - Defines the buffer size of the channel that connects
    ///                      `UdpConnection` and `UdpServerStream`. Both sides drop data/return
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
            connections: &mut HashMap<SocketAddr, UdpConnection>,
            socket: &mut UdpSocket,
        ) -> Option<(Bytes, SocketAddr)> {
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

    /// Creates a new `UdpConnection` and the connected `UdpServerStream`
    fn create_connection_and_stream(
        buffer_size: usize,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (UdpConnection, UdpServerStream) {
        let (con_sender, con_receiver) = channel(buffer_size);
        let (stream_sender, stream_receiver) = channel(buffer_size);

        (
            UdpConnection::new(stream_receiver, con_sender),
            UdpServerStream::new(con_receiver, stream_sender, local_addr, remote_addr),
        )
    }

    fn connect(&mut self, addr: SocketAddr) -> UdpServerStream {
        let (mut con, stream) = Self::create_connection_and_stream(
            self.buffer_size,
            self.socket.local_addr().unwrap(),
            addr,
        );
        self.connections.insert(addr, con);
        stream
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.local_addr().chain_err(|| "error")
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
                Occupied(mut entry) => entry.get_mut().recv(Bytes::from(&self.buf[..len])),
                Vacant(entry) => {
                    let (mut con, stream) = Self::create_connection_and_stream(
                        self.buffer_size,
                        self.socket.local_addr().unwrap(),
                        addr,
                    );
                    entry.insert(con).recv(Bytes::from(&self.buf[..len]));

                    self.new_connection.start_send((stream, addr));
                    self.new_connection.poll_complete();
                }
            };
        }
    }
}

/// UdpStream that is created by a `UdpServer` and is connected to a `UdpConnection`.
pub struct UdpServerStream {
    /// The sender to send data to the connected `UdpConnection` and effectively over the socket
    sender: Sender<Bytes>,
    /// The receiver to recv data from the connected `UdpConnection`
    receiver: Receiver<Bytes>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl UdpServerStream {
    /// Creates a new UdpServerStream
    fn new(
        receiver: Receiver<Bytes>,
        sender: Sender<Bytes>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> UdpServerStream {
        UdpServerStream {
            receiver,
            sender,
            local_addr,
            remote_addr,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl Stream for UdpServerStream {
    type Item = Bytes;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.receiver.poll()
    }
}

impl Sink for UdpServerStream {
    type SinkItem = Bytes;
    type SinkError = <Sender<Bytes> as Sink>::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
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
        if let AsyncSink::NotReady(_) = self.start_send(Bytes::from(buf)).map_err(to_io_error)? {
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

pub fn connect_async(
    connect: SocketAddr,
    handle: &Handle,
) -> FutureResult<UdpConnectStream, Error> {
    UdpSocket::bind(&([0, 0, 0, 0], 0).into(), handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| {
            socket
                .connect(&connect)
                .chain_err(|| format!("error connecting to {:?}", connect))
                .map(|_| socket)
        })
        .and_then(|socket| Ok(UdpConnectStream::new(socket)))
        .into_future()
}

/*
pub fn accept_async(
    listen_addr: SocketAddr,
    handle: &Handle,
    channel_buffer: usize,
) -> FutureResult<UdpServer, Error> {
    UdpSocket::bind(&listen_addr, handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| Ok(UdpServer::new(socket, channel_buffer, handle)))
        .into_future()
}
*/
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
        self.0.poll().chain_err(|| "error waiting for connect")
    }
}
/*
pub struct ConnectUdpServer {
    server: UdpServer,
    connect_recv: Fuse<UnboundedReceiver<SocketAddr>>,
}

impl ConnectUdpServer {
    fn new(socket: UdpSocket, channel_buffer: usize) -> (ConnectUdpServer, Connect) {
        let server = UdpServer::new(socket, channel_buffer);
        let (sender, recv) = unbounded();

        let server = ConnectUdpServer {
            server,
            connect_recv: recv.fuse(),
        };

        let connect = Connect {
            connect_sender: sender,
        };

        (server, connect)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }
}
*/
/// The type of a stream the `ConnectUdpServer` provides.
pub enum StreamType {
    /// The stream was created by connecting to a remote address.
    Connect,
    /// The stream was created by accepting a connection from a remote address.
    Accept,
}

/*
impl Stream for ConnectUdpServer {
    type Item = (UdpServerStream, SocketAddr, StreamType);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.connect_recv.poll() {
            Ok(Ready(Some(addr))) => Ok(Ready(
                Some((self.server.connect(addr), addr, StreamType::Connect)),
            )),
            _ => self.server.poll().map(|async| {
                async.map(|option| option.map(|v| (v.0, v.1, StreamType::Accept)))
            }),
        }
    }
}
*/

pub fn connect_and_accept_async(
    listen_addr: SocketAddr,
    handle: &Handle,
    channel_buffer: usize,
) -> Result<(UdpServer, Connect)> {
    UdpSocket::bind(&listen_addr, handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| Ok(UdpServer::new(socket, channel_buffer, handle)))
}
