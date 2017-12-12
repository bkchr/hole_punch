use errors::*;
use protocol::Protocol;

use std::net::SocketAddr;
use std::io::{self, Read, Write};

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream};

use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use serde::{Deserialize, Serialize};

mod udp_strat;

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
    pub fn connect<P>(&mut self, addr: SocketAddr) -> WaitForConnect<P>
    where
        P: Serialize + for<'de> Deserialize<'de>,
    {
        match self {
            &Connect::Udp(ref mut connect) => connect.connect(addr),
        }
    }
}

pub enum WaitForConnect<P> {
    Udp(udp_strat::WaitForConnect<P>),
}

impl<P> Future for WaitForConnect<P> {
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
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Strategy::Udp(ref mut server) => server.poll(),
        }
    }
}

impl<P> Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match self {
            &mut Strategy::Udp(ref server) => server.local_addr(),
        }
    }
}

pub enum Connection<P> {
    Udp(udp_strat::Connection<P>),
}

impl<P> Stream for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
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
    P: Serialize + for<'de> Deserialize<'de>,
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

impl<P> Connection<P> {
    pub fn into_pure(self) -> PureConnection {
        match self {
            Connection::Udp(mut stream) => {
                PureConnection::Udp(stream.into_inner().into_inner().into_inner())
            }
        }
    }
}

pub fn accept<P>(handle: &Handle) -> Result<Vec<Strategy<P>>> {
    let udp = udp_strat::accept_async(handle).chain_err(|| "error creating udp strategy")?;

    Ok(vec![udp])
}

pub fn connect<P>(handle: &Handle) -> Result<Vec<Strategy<P>>> {
    let udp = udp_strat::connect_async(handle).chain_err(|| "error creating udp strategy")?;

    Ok(vec![udp])
}

pub enum PureConnection {
    Udp(udp_strat::PureConnection),
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
