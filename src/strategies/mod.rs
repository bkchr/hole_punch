use errors::*;
use protocol::Protocol;

use std::net::SocketAddr;
use std::io::{self, Read, Write};
use std::marker::PhantomData;

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream};
use futures::sync::oneshot;

use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

use bytes::{Bytes, BytesMut};

mod udp;

pub enum ConnectionType {
    /// The connection was created by an incoming connection from a remote address
    Incoming,
    /// The connection was created by connecting to a remote address
    Outgoing,
}

#[derive(Clone)]
pub enum Connect {
    Udp(udp_strat::Connect),
}

impl Connect {
    pub fn connect<P>(&mut self, addr: SocketAddr) -> Result<WaitForConnect<P>>
    where
        P: Serialize + for<'de> Deserialize<'de> + Clone,
    {
        match self {
            &mut Connect::Udp(ref mut connect) => connect.connect(addr),
        }
    }
}

pub enum WaitForConnect<P> {
    Udp(udp_strat::WaitForConnect<P>),
}

impl<P> Future for WaitForConnect<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            &mut WaitForConnect::Udp(ref mut wait) => wait.poll(),
        }
    }
}

pub enum Strategy<P> {
    Udp(udp_strat::Server<P>),
}

impl<P> Stream for Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Strategy::Udp(ref mut server) => server.poll(),
        }
    }
}

impl<P> Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match self {
            &Strategy::Udp(ref server) => server.local_addr(),
        }
    }
}

pub enum Connection<P> {
    Udp(udp_strat::Connection<P>),
}

impl<P> Connection<P> {
    pub fn local_addr(&self) -> SocketAddr {
        match *self {
            Connection::Udp(ref con) => con.get_ref().get_ref().local_addr(),
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        match *self {
            Connection::Udp(ref con) => con.get_ref().get_ref().remote_addr(),
        }
    }
}

impl<P> Stream for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Protocol<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Connection::Udp(ref mut stream) => stream
                .poll()
                .chain_err(|| "error polling udp strategy connection"),
        }
    }
}

impl<P> Sink for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type SinkItem = Protocol<P>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self {
            &mut Connection::Udp(ref mut sink) => sink.start_send(item)
                .map_err(|_| "error at start_send on udp connection".into()),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self {
            &mut Connection::Udp(ref mut sink) => sink.poll_complete()
                .map_err(|_| "error at poll_complete on udp connection".into()),
        }
    }
}

impl<P> Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn into_pure(self) -> PureConnection {
        match self {
            Connection::Udp(stream) => PureConnection::Udp(stream.into_inner().into_inner()),
        }
    }

    pub fn send_and_poll(&mut self, msg: Protocol<P>) {
        if self.start_send(msg).is_err() || self.poll_complete().is_err() {
            eprintln!("error at `send_and_poll`");
        }
    }

    pub fn new_session(&self) -> NewSessionWait<P> {
        match *self {
            Connection::Udp(ref stream) => {
                NewSessionWait::Udp(stream.get_ref().get_ref().new_session(), Default::default())
            }
        }
    }

    pub fn new_session_controller(&self) -> NewSessionController {
        match *self {
            Connection::Udp(ref stream) => {
                NewSessionController::Udp(stream.get_ref().get_ref().new_session_controller())
            }
        }
    }
}

pub enum NewSessionWait<P> {
    Udp(udp_strat::NewSessionWait, PhantomData<P>),
}

impl<P> Future for NewSessionWait<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match *self {
            NewSessionWait::Udp(ref mut wait, _) => wait.poll()
                .map(|r| r.map(|v| Connection::Udp(WriteJson::new(ReadJson::new(v))))),
        }
    }
}

#[derive(Clone)]
pub enum NewSessionController {
    Udp(udp_strat::NewSession),
}

impl NewSessionController {
    pub fn new_session<P>(&self) -> NewSessionWait<P> {
        match *self {
            NewSessionController::Udp(ref new) => {
                let (sender, recv) = oneshot::channel();
                new.unbounded_send(sender);

                NewSessionWait::Udp(udp_strat::NewSessionWait::new(recv), Default::default())
            }
        }
    }
}

pub fn accept<P>(handle: &Handle) -> Result<Vec<Strategy<P>>>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let udp = udp_strat::accept_async(handle).chain_err(|| "error creating udp strategy")?;

    Ok(vec![udp])
}

pub fn connect<P>(handle: &Handle) -> Result<(Vec<Strategy<P>>, Vec<Connect>)>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let udp = udp_strat::connect_async(handle).chain_err(|| "error creating udp strategy")?;

    Ok((vec![udp.0], vec![udp.1]))
}

pub enum PureConnection {
    Udp(udp_strat::PureConnection),
}

impl Stream for PureConnection {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match *self {
            PureConnection::Udp(ref mut con) => con.poll(),
        }
    }
}

impl Sink for PureConnection {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self {
            &mut PureConnection::Udp(ref mut sink) => sink.start_send(item),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self {
            &mut PureConnection::Udp(ref mut sink) => sink.poll_complete(),
        }
    }
}

impl Read for PureConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            &mut PureConnection::Udp(ref mut con) => con.read(buf),
        }
    }
}

impl Write for PureConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            &mut PureConnection::Udp(ref mut con) => con.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            &mut PureConnection::Udp(ref mut con) => Write::flush(con),
        }
    }
}

impl AsyncRead for PureConnection {}

impl AsyncWrite for PureConnection {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match self {
            &mut PureConnection::Udp(ref mut con) => con.shutdown(),
        }
    }
}
