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

pub type PureConnection = udp::UdpServerStream;

impl<P> From<Connection<P>> for strategies::Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn from(value: Connection<P>) -> strategies::Connection<P> {
        strategies::Connection::Udp(value)
    }
}

pub struct Server<P> {
    server: udp::UdpServer,
    marker: PhantomData<P>,
}

impl<P> Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }
}

impl<P> From<udp::UdpServerStream> for strategies::Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn from(val: udp::UdpServerStream) -> strategies::Connection<P> {
        strategies::Connection::Udp(WriteBincode::new(ReadBincode::new(
            length_delimited::Framed::new(val),
        )))
    }
}

impl<P> Stream for Server<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (strategies::Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.server.poll() {
            Ok(Ready(Some(con))) => return Ok(Ready(Some((con.0.into(), con.1)))),
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
    };

    Ok(strategies::Strategy::Udp(server))
}

#[derive(Clone)]
pub struct Connect(udp::Connect);

impl Connect {
    pub fn connect<P>(&mut self, addr: SocketAddr) -> Result<strategies::WaitForConnect<P>>
    where
        P: Serialize + for<'de> Deserialize<'de>,
    {
        Ok(strategies::WaitForConnect::Udp(WaitForConnect(
            self.0.connect(addr)?,
            Default::default(),
        )))
    }
}

pub struct WaitForConnect<P>(udp::WaitForConnect, PhantomData<P>);

impl<P> Future for WaitForConnect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (strategies::Connection<P>, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().map(|v| {
            v.map(|v| {
                let port = v.get_local_addr().port();
                (v.into(), port)
            })
        })
    }
}

pub fn connect_async<P>(handle: &Handle) -> Result<(strategies::Strategy<P>, strategies::Connect)> {
    let (server, connect) = udp::connect_and_accept_async(([0, 0, 0, 0], 0).into(), handle, 4)?;

    let server = Server {
        server: server,
        marker: Default::default(),
    };

    Ok((
        strategies::Strategy::Udp(server),
        strategies::Connect::Udp(Connect(connect)),
    ))
}
