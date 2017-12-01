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

pub type Connection<P> = WriteBincode<
    ReadBincode<length_delimited::Framed<udp::UdpServerStream>, Protocol<P>>,
    Protocol<P>,
>;

impl<P> From<Connection<P>> for strategies::Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn from(value: Connection<P>) -> strategies::Connection<P> {
        strategies::Connection::Udp(value)
    }
}

enum FutureResultResolver<R>
where
    R: Future,
{
    Waiting(R),
    Resolved(<R as Future>::Item),
}

pub struct Server<P> {
    server: FutureResultResolver<FutureResult<(udp::ConnectUdpServer, udp::Connect), Error>>,
    marker: PhantomData<P>,
    connect_to: Vec<SocketAddr>,
}

impl From<udp::StreamType> for strategies::ConnectionType {
    fn from(val: udp::StreamType) -> strategies::ConnectionType {
        match val {
            udp::StreamType::Accept => strategies::ConnectionType::Incoming,
            udp::StreamType::Connect => strategies::ConnectionType::Outgoing,
        }
    }
}

impl<P> Server<P>
    where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match self.server {
            FutureResultResolver::Resolved((ref server, _)) => server.local_addr(),
            _ => bail!("Not bound to any port!"),
        }
    }
}

impl<P> Stream for Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr, strategies::ConnectionType);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let state = match self.server {
                FutureResultResolver::Waiting(ref mut f) => match f.poll()? {
                    Ready(s) => {
                        for addr in &self.connect_to {
                            s.1.connect(*addr).expect("Should not happen!")
                        }
                        self.connect_to.clear();
                        println!("RESOLVED");
                        FutureResultResolver::Resolved(s)
                    }
                    NotReady => return Ok(NotReady),
                },
                FutureResultResolver::Resolved((ref mut server, _)) => match server.poll() {
                    Ok(Ready(Some(con))) => {
                        return Ok(Ready(Some((
                            WriteBincode::new(
                                ReadBincode::new(length_delimited::Framed::new(con.0)),
                            ),
                            con.1,
                            con.2.into(),
                        ))))
                    }
                    r @ _ => {
                        return r.map(|r| r.map(|_| None))
                            .chain_err(|| "error polling UdpServer")
                    }
                },
            };

            self.server = state;
        }
    }
}

impl<P> strategies::ConnectTo for Server<P> {
    fn connect(&mut self, addr: SocketAddr) {
        match self.server {
            FutureResultResolver::Waiting(_) => self.connect_to.push(addr),
            FutureResultResolver::Resolved((_, ref connector)) => connector
                .connect(addr)
                .expect("Error sending connect request to Udp Server"),
        }
    }
}

pub fn accept_async<P>(handle: &Handle) -> strategies::Strategy<P> {
    let udp = udp::connect_and_accept_async(([0, 0, 0, 0], 22222).into(), handle, 4);

    let server = Server {
        server: FutureResultResolver::Waiting(udp),
        marker: Default::default(),
        connect_to: Vec::new(),
    };

    strategies::Strategy::Udp(server)
}

pub fn connect_async<P>(handle: &Handle) -> strategies::Strategy<P> {
    let udp = udp::connect_and_accept_async(([0, 0, 0, 0], 0).into(), handle, 4);

    let server = Server {
        server: FutureResultResolver::Waiting(udp),
        marker: Default::default(),
        connect_to: Vec::new(),
    };

    strategies::Strategy::Udp(server)
}
