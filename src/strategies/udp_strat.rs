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

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream};

use tokio_io::codec::length_delimited;

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

pub type Connection<P> = ReliableConnection<P>;

pub type PureConnection = udp::UdpServerStream;

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
                (strategies::Connection::Udp(ReliableConnection::new(v, &self.1)), port)
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

struct LengthDelimitedWrapper(length_delimited::Framed<udp::UdpServerStream>);

impl Stream for LengthDelimitedWrapper {
    type Item = <length_delimited::Framed<udp::UdpServerStream> as Stream>::Item;
    type Error = <length_delimited::Framed<udp::UdpServerStream> as Stream>::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.0.poll() {
            Err(ref err)
                if err.get_ref()
                    .map(|e| e.is::<length_delimited::FrameTooBig>())
                    .unwrap_or(false) =>
            {
                Ok(NotReady)
            }
            r @ _ => r,
        }
    }
}

impl Sink for LengthDelimitedWrapper {
    type SinkItem = <length_delimited::Framed<udp::UdpServerStream> as Sink>::SinkItem;
    type SinkError = <length_delimited::Framed<udp::UdpServerStream> as Sink>::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.0.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.0.poll_complete()
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum ReliableMsg<P> {
    Ack,
    Msg(Protocol<P>),
}

struct SortedRecv<P>(u64, Protocol<P>);

impl<P> PartialEq for SortedRecv<P> {
    fn eq(&self, other: &SortedRecv<P>) -> bool {
        self.0 == other.0
    }
}

impl<P> Eq for SortedRecv<P> {}

impl<P> PartialOrd for SortedRecv<P> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        other.0.partial_cmp(&self.0)
    }
}

impl<P> Ord for SortedRecv<P> {
    fn cmp(&self, other: &SortedRecv<P>) -> Ordering {
        let ord = self.partial_cmp(other).unwrap();

        // BinaryHeap is max heap, but we require min heap
        match ord {
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
            Ordering::Equal => ord,
        }
    }
}

pub struct ReliableConnection<P> {
    inner: WriteJson<
        ReadJson<LengthDelimitedWrapper, (u64, ReliableMsg<P>)>,
        (u64, ReliableMsg<P>),
    >,
    send_msgs_without_ack: HashMap<u64, (Protocol<P>, Timeout)>,
    next_id: u64,
    // the last id that was propagated upwards
    last_propagated_id: u64,
    received_heap: BinaryHeap<SortedRecv<P>>,
    handle: Handle,
}

impl<P> ReliableConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        con: udp::UdpServerStream,
        handle: &Handle,
    ) -> ReliableConnection<P> {
        let inner = WriteJson::new(ReadJson::new(LengthDelimitedWrapper(length_delimited::Framed::new(con))));

        ReliableConnection {
            inner,
            send_msgs_without_ack: HashMap::new(),
            next_id: 1,
            last_propagated_id: 0,
            received_heap: BinaryHeap::new(),
            handle: handle.clone(),
        }
    }

    fn send_ack(&mut self, id: u64) {
        self.inner.start_send((id, ReliableMsg::Ack));
        self.inner.poll_complete();
    }

    pub fn into_pure(self) -> PureConnection {
        self.inner.into_inner().into_inner().0.into_inner()
    }
}

impl<P> Stream for ReliableConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Protocol<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        fn check_resend<P>(
            inner: &mut WriteJson<
                ReadJson<LengthDelimitedWrapper, (u64, ReliableMsg<P>)>,
                (u64, ReliableMsg<P>),
            >,
            sends: &mut HashMap<u64, (Protocol<P>, Timeout)>,
        ) where
            P: Serialize + for<'de> Deserialize<'de> + Clone,
        {
            sends
                .iter_mut()
                .for_each(|(id, &mut (ref msg, ref mut timeout))| {
                    if timeout.poll().is_err() {
                        println!("RESEND");
                        inner.start_send((*id, ReliableMsg::Msg(msg.clone())));
                        timeout.reset();
                    }
                });

            inner.poll_complete();
        }

        check_resend(&mut self.inner, &mut self.send_msgs_without_ack);

        if Some(self.last_propagated_id + 1) == self.received_heap.peek().map(|m| m.0) {
            let msg = self.received_heap.pop().unwrap();
            self.last_propagated_id = msg.0;

            return Ok(Ready(Some(msg.1)));
        }

        loop {
            let msg = try_ready!(self.inner.poll());

            match msg {
                Some((id, ReliableMsg::Ack)) => {
                    println!("ACK({})", id);
                    self.send_msgs_without_ack.remove(&id);
                }
                Some((id, ReliableMsg::Msg(msg))) => {
                    println!("SENDACK({})", id);
                    self.send_ack(id);

                    // if we already propagated the msg, we don't need to do anything, it just means
                    // that the other end does not received the ack.
                    if id > self.last_propagated_id {
                        if self.last_propagated_id + 1 == id {
                            println!("PROPAGATE");
                            self.last_propagated_id = id;
                            return Ok(Ready(Some(msg)));
                        } else {
                            self.received_heap.push(SortedRecv(id, msg));
                        }
                    }
                }
                None => return Ok(Ready(None)),
            };
        }
    }
}

impl<P> Sink for ReliableConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type SinkItem = Protocol<P>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let id = self.next_id;
        self.next_id += 1;

        let timeout = Timeout::new(Duration::from_millis(200), &self.handle);
        self.send_msgs_without_ack
            .insert(id, (item.clone(), timeout));

        self.inner
            .start_send((id, ReliableMsg::Msg(item)))
            .map(|r| {
                r.map(|v| match v.1 {
                    ReliableMsg::Ack => unreachable!(),
                    ReliableMsg::Msg(msg) => msg,
                })
            })
            .map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete().map_err(|e| e.into())
    }
}
