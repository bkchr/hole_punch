use errors::*;
use udp;
use protocol::Protocol;
use strategies;

use std::marker::PhantomData;
use std::net::SocketAddr;

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream};
use futures::future::FutureResult;

use tokio_io::codec::length_delimited;

use tokio_core::reactor::Handle;

use tokio_serde_bincode::{ReadBincode, WriteBincode};

use serde::{Deserialize, Serialize};

pub struct Connection<P> {
    stream: WriteBincode<
        ReadBincode<length_delimited::Framed<udp::UdpServerStream>, Protocol<P>>,
        Protocol<P>,
    >,
}

impl<P> Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(con: udp::UdpServerStream) -> Connection<P> {
        Connection {
            stream: WriteBincode::new(ReadBincode::new(length_delimited::Framed::new(con))),
        }
    }
}

impl<P> Stream for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = Protocol<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.stream
            .poll()
            .chain_err(|| "error polling UdpConnection")
    }
}

impl<P> From<Connection<P>> for strategies::Connection<P> {
    fn from(value: Connection<P>) -> strategies::Connection<P> {
        strategies::Connection::Udp(value)
    }
}

pub struct Server<P> {
    server: strategies::FuturePoller<FutureResult<udp::UdpServer, Error>>,
    marker: PhantomData<P>,
}

impl<P> Stream for Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.server.poll() {
            Ok(Ready(Some(con))) => Ok(Ready(Some((Connection::new(con.0), con.1)))),
            r @ _ => r.map(|r| r.map(|_| None))
                .chain_err(|| "error polling UdpServer"),
        }
    }
}

pub fn accept_async<P>(handle: &Handle) -> strategies::Strategy<P> {
    let udp = udp::accept_async(([0, 0, 0, 0], 22222).into(), handle, 4);

    let server = Server { server: udp.into(), marker: Default::default() };

    strategies::Strategy::Udp(server)
}
