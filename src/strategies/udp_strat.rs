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
use std::io;
use std::cmp::min;
use std::fmt;

use futures::Async::{NotReady, Ready};
use futures::{self, AsyncSink, Future, Poll, Sink, StartSend, Stream};

use tokio_io::codec::length_delimited;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

pub type Connection<P> = ReliableConnection<Protocol<P>>;

pub type PureConnection = ReliableConnection<Vec<u8>>;

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
                    strategies::Connection::Udp(ReliableConnection::new(con.0, &self.handle)),
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
                    strategies::Connection::Udp(ReliableConnection::new(v, &self.1)),
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

// TODO: We need to implement this on our own, with all required features (session, id, maybe crc)
// backed in.
struct LengthDelimitedWrapper(Option<length_delimited::Framed<udp::UdpServerStream>>);

impl Stream for LengthDelimitedWrapper {
    type Item = <length_delimited::Framed<udp::UdpServerStream> as Stream>::Item;
    type Error = <length_delimited::Framed<udp::UdpServerStream> as Stream>::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match self.0.as_mut().unwrap().poll() {
                Err(ref err)
                    if err.get_ref()
                        .map(|e| e.is::<length_delimited::FrameTooBig>())
                        .unwrap_or(false) =>
                {
                    let inner = self.0.take().unwrap().into_inner();
                    self.0 = Some(length_delimited::Framed::new(inner));
                    println!("TOOBIG");
                }
                r @ _ => return r,
            }
        }
    }
}

impl Sink for LengthDelimitedWrapper {
    type SinkItem = <length_delimited::Framed<udp::UdpServerStream> as Sink>::SinkItem;
    type SinkError = <length_delimited::Framed<udp::UdpServerStream> as Sink>::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.0.as_mut().unwrap().start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.0.as_mut().unwrap().poll_complete()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum ReliableMsg<M> {
    Ack { id: u64 },
    Msg { id: u64, session: u8, data: M },
}

struct SortedRecv<M> {
    id: u64,
    session: u8,
    data: M,
}

impl<M> PartialEq for SortedRecv<M> {
    fn eq(&self, other: &SortedRecv<M>) -> bool {
        self.id == other.id
    }
}

impl<M> Eq for SortedRecv<M> {}

impl<M> PartialOrd for SortedRecv<M> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        other.id.partial_cmp(&self.id)
    }
}

impl<M> Ord for SortedRecv<M> {
    fn cmp(&self, other: &SortedRecv<M>) -> Ordering {
        let ord = self.partial_cmp(other).unwrap();

        // BinaryHeap is max heap, but we require min heap
        match ord {
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
            Ordering::Equal => ord,
        }
    }
}

pub struct ReliableConnection<M> {
    inner: Option<WriteJson<ReadJson<LengthDelimitedWrapper, ReliableMsg<M>>, ReliableMsg<M>>>,
    send_msgs_without_ack: HashMap<u64, (ReliableMsg<M>, Timeout)>,
    next_id: u64,
    // the last id that was propagated upwards
    last_propagated_id: u64,
    received_heap: BinaryHeap<SortedRecv<M>>,
    handle: Handle,
    session_id: u8,
}

impl<P> ReliableConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(con: udp::UdpServerStream, handle: &Handle) -> ReliableConnection<P> {
        let inner = Some(WriteJson::new(ReadJson::new(LengthDelimitedWrapper(
            Some(length_delimited::Framed::new(con)),
        ))));

        ReliableConnection {
            inner,
            send_msgs_without_ack: HashMap::new(),
            next_id: 1,
            last_propagated_id: 0,
            received_heap: BinaryHeap::new(),
            handle: handle.clone(),
            session_id: 0,
        }
    }

    fn send_ack(&mut self, id: u64) {
        self.inner
            .as_mut()
            .unwrap()
            .start_send(ReliableMsg::Ack { id });
        self.inner.as_mut().unwrap().poll_complete();
    }

    pub fn into_pure(mut self) -> PureConnection {
        let con = self.inner
            .take()
            .unwrap()
            .into_inner()
            .into_inner()
            .0
            .take()
            .unwrap()
            .into_inner();
        let handle = self.handle;

        let mut connection = ReliableConnection::new(con, &handle);
        connection.session_id += 1;

        connection
    }
}

impl<M> Stream for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = M;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        fn check_resend<M>(
            inner: &mut WriteJson<ReadJson<LengthDelimitedWrapper, ReliableMsg<M>>, ReliableMsg<M>>,
            sends: &mut HashMap<u64, (ReliableMsg<M>, Timeout)>,
        ) where
            M: Serialize + for<'de> Deserialize<'de> + Clone,
        {
            sends
                .iter_mut()
                .for_each(|(id, &mut (ref msg, ref mut timeout))| {
                    if timeout.poll().is_err() {
                        println!("RESEND");
                        inner.start_send(msg.clone());
                        timeout.reset();
                        let _ = timeout.poll();
                    }
                });

            inner.poll_complete();
        }

        check_resend(
            self.inner.as_mut().unwrap(),
            &mut self.send_msgs_without_ack,
        );

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
            let msg = match self.inner.as_mut().unwrap().poll() {
                Err(e) => {
                    println!("ERROR: {:?}", e);

                    // HACK, really, really shitty, to reset the state we need to create a new instance..
                    let inner = self.inner
                        .take()
                        .unwrap()
                        .into_inner()
                        .into_inner()
                        .0
                        .take()
                        .unwrap()
                        .into_inner();
                    self.inner = Some(WriteJson::new(ReadJson::new(LengthDelimitedWrapper(
                        Some(length_delimited::Framed::new(inner)),
                    ))));

                    continue;
                }
                Ok(NotReady) => return Ok(NotReady),
                Ok(Ready(msg)) => msg,
            };

            match msg {
                Some(ReliableMsg::Ack { id }) => {
                    println!("ACK({})", id);
                    self.send_msgs_without_ack.remove(&id);
                }
                Some(ReliableMsg::Msg { id, session, data }) => {
                    //only send acks for messages in the same session
                    if session == self.session_id {
                        println!("SENDACK({})", id);
                        self.send_ack(id);
                    }

                    // if we already propagated the msg, we don't need to do anything, it just means
                    // that the other end does not received the ack.
                    if id > self.last_propagated_id {
                        if self.last_propagated_id + 1 == id {
                            println!("PROPAGATE");
                            self.last_propagated_id = id;

                            if session == self.session_id {
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
                            self.received_heap.push(SortedRecv { id, session, data });
                        }
                    }
                }
                None => return Ok(Ready(None)),
            };
        }
    }
}

impl<M> Sink for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type SinkItem = M;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = ReliableMsg::Msg {
            id,
            session: self.session_id,
            data: item,
        };

        let mut timeout = Timeout::new(Duration::from_millis(200), &self.handle);
        timeout.poll();
        self.send_msgs_without_ack
            .insert(id, (msg.clone(), timeout));

        println!("SEND({})", id);
        self.inner
            .as_mut()
            .unwrap()
            .start_send(msg)
            .map(|r| {
                r.map(|v| match v {
                    ReliableMsg::Ack { .. } => unreachable!(),
                    ReliableMsg::Msg { data, .. } => data,
                })
            })
            .map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner
            .as_mut()
            .unwrap()
            .poll_complete()
            .map_err(|e| e.into())
    }
}

fn to_io_error<E: fmt::Debug>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", error))
}

impl<M> io::Write for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone + AsRef<[u8]> + From<Vec<u8>>,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let AsyncSink::NotReady(_) = self.start_send(buf.to_vec().into()).map_err(to_io_error)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        self.poll_complete().map_err(to_io_error)?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<M> io::Read for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone + AsRef<[u8]>,
{
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

impl<M> AsyncRead for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone + AsRef<[u8]>,
{
}

impl<M> AsyncWrite for ReliableConnection<M>
where
    M: Serialize + for<'de> Deserialize<'de> + Clone + AsRef<[u8]> + From<Vec<u8>>,
{
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}
