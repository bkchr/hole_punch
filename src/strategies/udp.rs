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

use picoquic;

type Connection = picoquic::Stream;

pub struct Server {
    server: picoquic::Context,
    handle: Handle,
    new_session_inform_recv: UnboundedReceiver<UdpConnection>,
    new_session_inform_send: UnboundedSender<UdpConnection>,
}

impl Server {
    fn new(listen_address: SocketAddr, handle: Handle) -> Self {
        let (new_session_inform_send, new_session_inform_recv) = unbounded();

        Server {
            server,
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
                )))));
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


