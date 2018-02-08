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

pub enum Connection {
    Udp(udp_strat::Connection),
}

impl Connection {
    pub fn local_addr(&self) -> SocketAddr {
        match *self {
            Connection::Udp(ref con) => con.local_addr(),
        }
    }

    pub fn peer_addr(&self) -> SocketAddr {
        match *self {
            Connection::Udp(ref con) => con.peer_addr(),
        }
    }
}

impl Stream for Connection
{
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Connection::Udp(ref mut stream) => stream
                .poll()
                .chain_err(|| "error polling udp strategy connection"),
        }
    }
}

impl Sink for Connection
{
    type SinkItem = BytesMut;
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

impl NewSession for Connection {
    fn new_session(&mut self) -> NewConnectionFuture {
        match *self {
            Connection::Udp(ref mut sink) => sink.new_session()
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

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            &mut Connection::Udp(ref mut con) => con.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            &mut Connection::Udp(ref mut con) => con.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            &mut Connection::Udp(ref mut con) => Write::flush(con),
        }
    }
}

impl AsyncRead for Connection {}

impl AsyncWrite for Connection {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match self {
            &mut Connection::Udp(ref mut con) => con.shutdown(),
        }
    }
}

pub trait NewSession {
    fn new_session(&mut self) -> NewConnectionFuture;
}

pub trait NewConnection {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture;
}

pub struct NewSessionFuture {
    inner: Box<Future<Item=Connection>>
}

impl Future for NewConnectionFuture {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

