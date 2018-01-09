use errors::*;
use udp;
use protocol::Protocol;
use strategies;
use timeout::Timeout;

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::collections::{BinaryHeap, HashMap};
use std::cmp::Ordering;
use std::time::Duration;
use std::io::{self, Cursor};
use std::cmp::min;
use std::fmt;
use std::mem::{size_of, size_of_val};

use futures::Async::{NotReady, Ready};
use futures::{self, AsyncSink, Future, Poll, Sink, StartSend, Stream};

use tokio_io::codec::length_delimited;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

use bytes::{BigEndian, Buf, BufMut, Bytes, BytesMut};

pub type Connection<P> = WriteJson<ReadJson<ReliableConnection, Protocol<P>>, Protocol<P>>;

pub type PureConnection = ReliableConnection;

pub struct Server<P> {
    server: udp::UdpServer,
    marker: PhantomData<P>,
    handle: Handle,
}

impl<P> Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }
}

impl<P> Stream for Server<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (strategies::Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.server.poll() {
            Ok(Ready(Some(con))) => {
                return Ok(Ready(Some((
                    strategies::Connection::Udp(WriteJson::new(ReadJson::new(
                        ReliableConnection::new(con.0, &self.handle),
                    ))),
                    con.1,
                ))))
            }
            r @ _ => {
                return r.map(|r| r.map(|_| None))
                    .chain_err(|| "error polling UdpServer")
            }
        }
    }
}

pub fn accept_async<P>(handle: &Handle) -> Result<strategies::Strategy<P>> {
    let (server, _) = udp::connect_and_accept_async(([0, 0, 0, 0], 22222).into(), handle, 4)?;

    let server = Server {
        server: server,
        marker: Default::default(),
        handle: handle.clone(),
    };

    Ok(strategies::Strategy::Udp(server))
}

#[derive(Clone)]
pub struct Connect(udp::Connect, Handle);

impl Connect {
    pub fn connect<P>(&mut self, addr: SocketAddr) -> Result<strategies::WaitForConnect<P>>
    where
        P: Serialize + for<'de> Deserialize<'de>,
    {
        Ok(strategies::WaitForConnect::Udp(WaitForConnect(
            self.0.connect(addr)?,
            self.1.clone(),
            Default::default(),
        )))
    }
}

pub struct WaitForConnect<P>(udp::WaitForConnect, Handle, PhantomData<P>);

impl<P> Future for WaitForConnect<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (strategies::Connection<P>, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().map(|v| {
            v.map(|v| {
                let port = v.get_local_addr().port();
                (
                    strategies::Connection::Udp(WriteJson::new(ReadJson::new(
                        ReliableConnection::new(v, &self.1),
                    ))),
                    port,
                )
            })
        })
    }
}

pub fn connect_async<P>(handle: &Handle) -> Result<(strategies::Strategy<P>, strategies::Connect)> {
    let (server, connect) = udp::connect_and_accept_async(([0, 0, 0, 0], 0).into(), handle, 4)?;

    let server = Server {
        server: server,
        marker: Default::default(),
        handle: handle.clone(),
    };

    Ok((
        strategies::Strategy::Udp(server),
        strategies::Connect::Udp(Connect(connect, handle.clone())),
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
    session: u8,
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
    // length of the entire package, version as u16, id as u64, session id as u8, ty as u8
    size_of::<u64>() + size_of::<u16>() + size_of::<u64>() + size_of::<u8>() + size_of::<u8>()
}

fn pack_reliable_msg(id: u64, session_id: u8, ty: ReliableMsgType, data: &[u8]) -> Bytes {
    let len = get_reliable_msg_header_len() + data.len();
    let mut bytes = BytesMut::with_capacity(get_reliable_msg_header_len() + data.len());

    bytes.put_u64::<BigEndian>(len as u64);
    bytes.put_u16::<BigEndian>(0);
    bytes.put_u64::<BigEndian>(id);
    bytes.put_u8(session_id);
    bytes.put_u8(ty as u8);
    bytes.put_slice(data);

    bytes.freeze()
}

fn unpack_reliable_msg(mut data: Bytes) -> Result<(u64, u8, ReliableMsgType, Option<Bytes>)> {
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
        src.get_u16::<BigEndian>();

        let id = src.get_u64::<BigEndian>();

        let session_id = src.get_u8();

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

pub struct ReliableConnection {
    con: udp::UdpServerStream,
    send_msgs_without_ack: HashMap<u64, (Bytes, Timeout)>,
    next_id: u64,
    // the last id that was propagated upwards
    last_propagated_id: u64,
    received_heap: BinaryHeap<SortedRecv>,
    handle: Handle,
    session_id: u8,
}

impl ReliableConnection {
    fn new(con: udp::UdpServerStream, handle: &Handle) -> ReliableConnection {
        ReliableConnection {
            con,
            send_msgs_without_ack: HashMap::new(),
            next_id: 1,
            last_propagated_id: 0,
            received_heap: BinaryHeap::new(),
            handle: handle.clone(),
            session_id: 0,
        }
    }

    fn send_ack(&mut self, id: u64) {
        self.con.start_send(pack_reliable_msg(
            id,
            self.session_id,
            ReliableMsgType::Ack,
            &[],
        ));
        self.con.poll_complete();
    }

    pub fn into_pure(mut self) -> PureConnection {
        let con = self.con;
        let handle = self.handle;

        let mut connection = ReliableConnection::new(con, &handle);
        connection.session_id += 1;

        connection
    }
}

impl Stream for ReliableConnection {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        fn check_resend(
            con: &mut udp::UdpServerStream,
            sends: &mut HashMap<u64, (Bytes, Timeout)>,
        ) {
            sends
                .iter_mut()
                .for_each(|(id, &mut (ref msg, ref mut timeout))| {
                    if timeout.poll().is_err() {
                        con.start_send(msg.clone());
                        timeout.reset();
                        let _ = timeout.poll();
                    }
                });

            con.poll_complete();
        }

        check_resend(&mut self.con, &mut self.send_msgs_without_ack);

        loop {
            if Some(self.last_propagated_id + 1) == self.received_heap.peek().map(|m| m.id) {
                let msg = self.received_heap.pop().unwrap();
                self.last_propagated_id = msg.id;

                if self.session_id == msg.session {
                    return Ok(Ready(Some(msg.data)));
                }
            } else {
                break;
            }
        }

        loop {
            let msg = match self.con.poll() {
                Err(_) => bail!("error polling UdpServerStream"),
                Ok(Ready(Some(msg))) => msg,
                Ok(Ready(None)) => return Ok(Ready(None)),
                Ok(NotReady) => return Ok(NotReady),
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
                    self.send_msgs_without_ack.remove(&id);
                }
                (ReliableMsgType::Msg, Some(data)) => {
                    //only send acks for messages in the same session
                    if session_id == self.session_id {
                        self.send_ack(id);
                    }

                    // if we already propagated the msg, we don't need to do anything, it just means
                    // that the other end does not received the ack.
                    if id > self.last_propagated_id {
                        if self.last_propagated_id + 1 == id {
                            //println!("PROPAGATE");
                            self.last_propagated_id = id;

                            if session_id == self.session_id {
                                return Ok(Ready(Some(data)));
                            } else {
                                loop {
                                    if Some(self.last_propagated_id + 1)
                                        == self.received_heap.peek().map(|m| m.id)
                                    {
                                        let msg = self.received_heap.pop().unwrap();
                                        self.last_propagated_id = msg.id;

                                        if self.session_id == msg.session {
                                            return Ok(Ready(Some(msg.data)));
                                        }
                                    } else {
                                        break;
                                    }
                                }
                            }
                        } else {
                            // TODO: same id could be inserted multiple times
                            self.received_heap.push(SortedRecv {
                                id,
                                session: session_id,
                                data,
                            });
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

impl Sink for ReliableConnection {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = pack_reliable_msg(id, self.session_id, ReliableMsgType::Msg, &item);

        let mut timeout = Timeout::new(Duration::from_millis(200), &self.handle);
        timeout.poll();
        self.send_msgs_without_ack
            .insert(id, (msg.clone(), timeout));

        self.con
            .start_send(msg)
            .map(|r| r.map(|_| item))
            .map_err(|_| "error at ReliableConnection::start_send".into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.con
            .poll_complete()
            .map_err(|_| "error at ReliableConnection::poll_complete".into())
    }
}

fn to_io_error<E: fmt::Debug>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", error))
}

impl io::Write for ReliableConnection {
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

impl io::Read for ReliableConnection {
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

impl AsyncRead for ReliableConnection {}

impl AsyncWrite for ReliableConnection {
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}
