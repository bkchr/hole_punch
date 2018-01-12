use errors::*;
use udp;
use protocol::Protocol;
use strategies;
use timeout::Timeout;

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::collections::{BinaryHeap, HashMap};
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cmp::Ordering;
use std::time::Duration;
use std::io::{self, Cursor};
use std::cmp::min;
use std::fmt;
use std::mem::{size_of, size_of_val};

use futures::Async::{NotReady, Ready};
use futures::{self, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use futures::sync::mpsc::{channel, unbounded, Receiver, SendError, Sender, UnboundedReceiver,
                          UnboundedSender};
use futures::sync::oneshot;

use tokio_io::codec::length_delimited;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

use bytes::{BigEndian, Buf, BufMut, Bytes, BytesMut};

pub type Connection<P> = WriteJson<ReadJson<UdpConnection, Protocol<P>>, Protocol<P>>;

pub type PureConnection = UdpConnection;

pub struct Server<P> {
    server: udp::UdpServer,
    marker: PhantomData<P>,
    handle: Handle,
    new_session_inform_recv: UnboundedReceiver<UdpConnection>,
    new_session_inform_send: UnboundedSender<UdpConnection>,
}

impl<P> Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(server: udp::UdpServer, handle: Handle) -> Self {
        let (new_session_inform_send, new_session_inform_recv) = unbounded();

        Server {
            server,
            marker: Default::default(),
            handle,
            new_session_inform_recv,
            new_session_inform_send,
        }
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }

    fn get_new_session_inform(&self) -> UnboundedSender<UdpConnection> {
        self.new_session_inform_send.clone()
    }
}

impl<P> Stream for Server<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = strategies::Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.new_session_inform_recv.poll() {
            Ok(Ready(Some(con))) => {
                return Ok(Ready(Some(strategies::Connection::Udp(WriteJson::new(
                    ReadJson::new(con),
                )))))
            }
            _ => {}
        };

        self.server
            .poll()
            .map(|r| {
                r.map(|o| {
                    o.map(|con| {
                        let local_addr = con.0.local_addr();
                        let (rel_con, con) = ReliableConnection::new(
                            con.0,
                            self.new_session_inform_send.clone(),
                            &self.handle,
                            local_addr,
                            con.1,
                        );

                        self.handle.spawn(rel_con);

                        strategies::Connection::Udp(WriteJson::new(ReadJson::new(con)))
                    })
                })
            })
            .chain_err(|| "error polling UdpServer")
    }
}

pub fn accept_async<P>(handle: &Handle) -> Result<strategies::Strategy<P>>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let (server, _) = udp::connect_and_accept_async(([0, 0, 0, 0], 22222).into(), handle, 4)?;

    let server = Server::new(server, handle.clone());
    Ok(strategies::Strategy::Udp(server))
}

#[derive(Clone)]
pub struct Connect(udp::Connect, Handle, UnboundedSender<UdpConnection>);

impl Connect {
    pub fn connect<P>(&mut self, addr: SocketAddr) -> Result<strategies::WaitForConnect<P>>
    where
        P: Serialize + for<'de> Deserialize<'de>,
    {
        Ok(strategies::WaitForConnect::Udp(WaitForConnect(
            self.0.connect(addr)?,
            self.1.clone(),
            self.2.clone(),
            Default::default(),
        )))
    }
}

pub struct WaitForConnect<P>(
    udp::WaitForConnect,
    Handle,
    UnboundedSender<UdpConnection>,
    PhantomData<P>,
);

impl<P> Future for WaitForConnect<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = strategies::Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().map(|v| {
            v.map(|con| {
                let remote_addr = con.remote_addr();
                let local_addr = con.local_addr();
                let (rel_con, con) =
                    ReliableConnection::new(con, self.2.clone(), &self.1, local_addr, remote_addr);

                self.1.spawn(rel_con);
                strategies::Connection::Udp(WriteJson::new(ReadJson::new(con)))
            })
        })
    }
}

pub fn connect_async<P>(handle: &Handle) -> Result<(strategies::Strategy<P>, strategies::Connect)>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let (server, connect) = udp::connect_and_accept_async(([0, 0, 0, 0], 0).into(), handle, 4)?;

    let server = Server::new(server, handle.clone());

    let session_inform = server.get_new_session_inform();

    Ok((
        strategies::Strategy::Udp(server),
        strategies::Connect::Udp(Connect(connect, handle.clone(), session_inform)),
    ))
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
    data: Bytes,
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

fn pack_reliable_msg(id: u64, session_id: u16, ty: ReliableMsgType, data: &[u8]) -> Bytes {
    let len = get_reliable_msg_header_len() + data.len();
    let mut bytes = BytesMut::with_capacity(get_reliable_msg_header_len() + data.len());

    bytes.put_u64::<BigEndian>(len as u64);
    bytes.put_u8(0);
    bytes.put_u64::<BigEndian>(id);
    bytes.put_u16::<BigEndian>(session_id);
    bytes.put_u8(ty as u8);
    bytes.put_slice(data);

    bytes.freeze()
}

fn unpack_reliable_msg(data: Bytes) -> Result<(u64, u16, ReliableMsgType, Option<Bytes>)> {
    let len = data.len();

    if len < get_reliable_msg_header_len() {
        bail!(
            "received data does not contain enough data to hold the header(length: {})",
            len
        );
    }

    let (data_len, id, session_id, ty) = {
        let mut src = Cursor::new(&data);

        let recv_len = src.get_u64::<BigEndian>();

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

        let id = src.get_u64::<BigEndian>();

        let session_id = src.get_u16::<BigEndian>();

        let ty = ReliableMsgType::try_from(src.get_u8())?;

        (
            recv_len as usize - get_reliable_msg_header_len(),
            id,
            session_id,
            ty,
        )
    };

    let data = if data_len > 0 {
        Some(data.slice(
            get_reliable_msg_header_len(),
            get_reliable_msg_header_len() + data_len,
        ))
    } else {
        None
    };

    Ok((id, session_id, ty, data))
}

type NewSession = UnboundedSender<oneshot::Sender<UdpConnection>>;

struct Session {
    sender: UnboundedSender<Bytes>,
    recv: UnboundedReceiver<BytesMut>,
    next_id: u64,
    session_id: u16,
    received_data: BinaryHeap<SortedRecv>,
    // the last id that was propagated
    last_propagated_id: u64,
    send_msgs_without_ack: HashMap<u64, (Bytes, Timeout)>,
}

impl Session {
    fn new(
        session_id: u16,
        new_session: NewSession,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (Session, UdpConnection) {
        let (sender, poll_recv) = unbounded();
        let (sink_sender, recv) = unbounded();

        (
            Session {
                recv,
                sender,
                next_id: 1,
                session_id,
                received_data: BinaryHeap::new(),
                last_propagated_id: 0,
                send_msgs_without_ack: HashMap::new(),
            },
            UdpConnection::new(poll_recv, sink_sender, new_session, local_addr, remote_addr),
        )
    }

    fn recv(&mut self, id: u64, data: Bytes) -> Result<()> {
        if self.last_propagated_id + 1 == id {
            self.last_propagated_id = id;

            self.sender
                .unbounded_send(data)
                .map_err(|_| "`Session::recv` send error")?;
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

    fn send(&mut self, handle: &Handle) -> Poll<Option<Bytes>, Error> {
        let data = match try_ready!(self.recv.poll().map_err(|_| "`Session::send` poll error")) {
            Some(data) => data,
            None => bail!("None received in `Session::send`"),
        };

        let id = self.next_id;
        self.next_id += 1;

        let data = pack_reliable_msg(id, self.session_id, ReliableMsgType::Msg, &data);
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

    fn check_resend(&mut self) -> Option<Vec<Bytes>> {
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
    next_session_id: u16,
    sessions: HashMap<u16, Session>,
    new_session_recv: UnboundedReceiver<oneshot::Sender<UdpConnection>>,
    new_session: NewSession,
    new_session_inform: UnboundedSender<UdpConnection>,
    known_sessions: Vec<u16>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl ReliableConnection {
    fn new(
        con: udp::UdpServerStream,
        new_session_inform: UnboundedSender<UdpConnection>,
        handle: &Handle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (ReliableConnection, UdpConnection) {
        let (new_session, new_session_recv) = unbounded();

        let mut con = ReliableConnection {
            con,
            handle: handle.clone(),
            next_session_id: 0,
            sessions: HashMap::new(),
            new_session_recv,
            new_session,
            new_session_inform,
            known_sessions: Vec::new(),
            local_addr,
            remote_addr,
        };

        // create the initial session
        let udp_con = con.create_new_session();

        (con, udp_con)
    }

    fn send_ack(&mut self, id: u64, session: u16) {
        self.con
            .start_send(pack_reliable_msg(id, session, ReliableMsgType::Ack, &[]));
        self.con.poll_complete();
    }

    fn create_new_session(&mut self) -> UdpConnection {
        let (session, con) = Session::new(
            self.next_session_id,
            self.new_session.clone(),
            self.local_addr,
            self.remote_addr,
        );

        self.sessions.insert(self.next_session_id, session);
        self.known_sessions.push(self.next_session_id);
        self.next_session_id += 1;

        con
    }

    fn check_for_new_sessions(&mut self) {
        loop {
            let new = match self.new_session_recv.poll() {
                Ok(Ready(Some(new))) => new,
                _ => return,
            };

            let con = self.create_new_session();
            let _ = new.send(con);
        }
    }

    fn check_send_data(&mut self) {
        fn retain(
            sessions: &mut HashMap<u16, Session>,
            stream: &mut udp::UdpServerStream,
            handle: &Handle,
        ) {
            sessions.retain(|_, s| {
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
                            println!("{:?}", e);
                            return false;
                        }
                    }
                }
            })
        }

        retain(&mut self.sessions, &mut self.con, &self.handle)
    }

    fn check_recv_data(&mut self) -> Poll<(), ()> {
        loop {
            let msg = match try_ready!(self.con.poll().map_err(|_| ())) {
                Some(msg) => msg,
                None => {
                    println!("ReliableConnection closed!");
                    return Err(());
                }
            };

            let (id, session_id, ty, data) = match unpack_reliable_msg(msg) {
                Err(e) => {
                    println!("Error: {:?}", e);
                    continue;
                }
                Ok(msg) => msg,
            };

            match (ty, data) {
                (ReliableMsgType::Ack, None) => {
                    self.sessions.get_mut(&session_id).map(|s| s.ack_recv(id));
                }
                (ReliableMsgType::Msg, Some(data)) => {
                    self.send_ack(id, session_id);

                    match self.sessions.entry(session_id) {
                        Occupied(mut session) => {
                            if session.get_mut().recv(id, data).is_err() {
                                session.remove_entry();
                            }
                        }
                        Vacant(entry) => {
                            if !self.known_sessions.contains(&session_id) {
                                let (mut session, con) = Session::new(
                                    session_id,
                                    self.new_session.clone(),
                                    self.local_addr,
                                    self.remote_addr,
                                );

                                let _ = session.recv(id, data);

                                entry.insert(session);
                                self.known_sessions.push(session_id);
                                self.next_session_id = match session_id.checked_add(1) {
                                    Some(val) => val,
                                    None => {
                                        println!("session_id out of bounce");
                                        return Err(());
                                    }
                                };

                                self.new_session_inform.unbounded_send(con);
                            } else {
                                println!("received package for session({}), after the session was finished!", session_id);
                            }
                        }
                    }
                }
                _ => {
                    println!("received weird shit and will ignore it!");
                }
            }
        }
    }
}

impl Future for ReliableConnection {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.check_for_new_sessions();
        self.check_send_data();
        self.check_recv_data()
    }
}

pub struct NewSessionWait(oneshot::Receiver<UdpConnection>);

impl Future for NewSessionWait {
    type Item = UdpConnection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0
            .poll()
            .map_err(|_| "error at `NewSessionWait::poll`".into())
    }
}

pub struct UdpConnection {
    recv: UnboundedReceiver<Bytes>,
    sender: UnboundedSender<BytesMut>,
    new_session: NewSession,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl UdpConnection {
    fn new(
        recv: UnboundedReceiver<Bytes>,
        sender: UnboundedSender<BytesMut>,
        new_session: UnboundedSender<oneshot::Sender<UdpConnection>>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> UdpConnection {
        UdpConnection {
            recv,
            sender,
            new_session,
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

    pub fn new_session(&self) -> NewSessionWait {
        let (sender, receiver) = oneshot::channel();

        self.new_session.unbounded_send(sender);

        NewSessionWait(receiver)
    }
}

impl Stream for UdpConnection {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.recv
            .poll()
            .map_err(|_| "error `UdpConnection::poll`".into())
    }
}

impl Sink for UdpConnection {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.sender
            .start_send(item)
            .map_err(|_| "error `UdpConnection::start_send`".into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sender
            .poll_complete()
            .map_err(|_| "error `UdpConnection::poll_complete`".into())
    }
}

fn to_io_error<E: fmt::Debug>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", error))
}

impl io::Write for UdpConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let AsyncSink::NotReady(_) = self.start_send(BytesMut::from(buf)).map_err(to_io_error)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        self.poll_complete().map_err(to_io_error)?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for UdpConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let res = self.poll().map_err(to_io_error)?;

        match res {
            NotReady => Err(io::ErrorKind::WouldBlock.into()),
            Ready(Some(data)) => {
                // If buf is too small, elements will get lost
                // TODO: maybe integrate tmp buffer for 'lost' elements.
                let len = min(buf.len(), data.as_ref().len());
                &buf[..len].copy_from_slice(&data.as_ref()[..len]);
                Ok(len)
            }
            Ready(None) => Ok(0),
        }
    }
}

impl AsyncRead for UdpConnection {}

impl AsyncWrite for UdpConnection {
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}
