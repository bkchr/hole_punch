use authenticator::Authenticator;
use config::Config;
use connect::{self, ConnectWithStrategies};
use connection_request;
use context::ConnectionId;
use error::*;
use incoming;
use protocol::Protocol;
use strategies::{self, AddressInformation, GetConnectionId, NewConnection, NewStream};

use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::{mpsc, oneshot};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use tokio_io::codec::length_delimited;

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use rand::{self, Rng};

use itertools::Itertools;

#[derive(Clone)]
pub struct NewStreamHandle<P> {
    new_stream_handle: strategies::NewStreamHandle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    is_p2p_con: bool,
    handle: Handle,
}

impl<P> NewStreamHandle<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        new_stream_handle: strategies::NewStreamHandle,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        is_p2p_con: bool,
        handle: &Handle,
    ) -> NewStreamHandle<P> {
        NewStreamHandle {
            new_stream_handle,
            connect_callback,
            is_p2p_con,
            handle: handle.clone(),
        }
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.new_stream_handle.new_stream(),
            self.clone(),
            self.connect_callback.clone(),
            self.is_p2p_con,
            &self.handle,
        )
    }
}

pub struct NewStreamFuture<P> {
    new_stream: strategies::NewStreamFuture,
    new_stream_handle: NewStreamHandle<P>,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    handle: Handle,
    is_p2p_con: bool,
}

impl<P> NewStreamFuture<P> {
    pub fn new(
        new_stream: strategies::NewStreamFuture,
        new_stream_handle: NewStreamHandle<P>,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        is_p2p_con: bool,
        handle: &Handle,
    ) -> NewStreamFuture<P> {
        NewStreamFuture {
            new_stream,
            new_stream_handle,
            connect_callback,
            is_p2p_con,
            handle: handle.clone(),
        }
    }
}

impl<P> Future for NewStreamFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_stream.poll().map(|r| {
            r.map(|v| {
                Stream::new(
                    v,
                    None,
                    &self.handle,
                    self.new_stream_handle.clone(),
                    self.connect_callback.clone(),
                    self.is_p2p_con,
                )
            })
        })
    }
}

pub fn get_interface_addresses(local_addr: SocketAddr) -> Vec<SocketAddr> {
    interfaces()
        .iter()
        .map(|v| v.ips.clone())
        .concat()
        .iter()
        .map(|v| v.ip())
        .filter(|ip| !ip.is_loopback())
        .map(|ip| (ip, local_addr.port()).into())
        .collect_vec()
}

enum StreamState {
    Authenticated,
    UnAuthenticated(oneshot::Sender<bool>),
}

enum HandleProtocol<P> {
    Send(Protocol<P>),
    AddressInfoRequest(connection_request::ConnectionRequestSlaveHandle),
}

pub struct Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    stream:
        WriteJson<ReadJson<length_delimited::Framed<strategies::Stream>, Protocol<P>>, Protocol<P>>,
    state: StreamState,
    stream_handle: StreamHandle<P>,
    stream_handle_recv: mpsc::UnboundedReceiver<HandleProtocol<P>>,
    con_requests: HashMap<ConnectionId, connection_request::ConnectionRequestMasterHandle>,
    incoming_con_requests: HashMap<ConnectionId, Vec<SocketAddr>>,
    address_info_requests: Vec<connection_request::ConnectionRequestSlaveHandle>,
    handle: Handle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    is_p2p_con: bool,
}

impl<P> Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        stream: strategies::Stream,
        auth_con: Option<oneshot::Sender<bool>>,
        handle: &Handle,
        new_stream_handle: NewStreamHandle<P>,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        is_p2p_con: bool,
    ) -> Stream<P> {
        let state = match auth_con {
            Some(auth) => StreamState::UnAuthenticated(auth),
            None => StreamState::Authenticated,
        };

        let (stream_handle_send, stream_handle_recv) = mpsc::unbounded();
        let stream_handle = StreamHandle::new(stream_handle_send, new_stream_handle);

        Stream {
            stream: WriteJson::new(ReadJson::new(length_delimited::Framed::new(stream))),
            state,
            con_requests: HashMap::new(),
            incoming_con_requests: HashMap::new(),
            address_info_requests: Vec::new(),
            handle: handle.clone(),
            connect_callback,
            stream_handle,
            stream_handle_recv,
            is_p2p_con,
        }
    }

    /// Upgrades this `Stream` and the underlying `Connection` to be authenticated.
    /// This allows the `Connection` to accept multiple `Stream`s and the `Stream`s can execute
    /// the full protocol, e.g. opening a connection to a different device.
    pub fn upgrade_to_authenticated(&mut self) {
        match mem::replace(&mut self.state, StreamState::Authenticated) {
            StreamState::Authenticated => {}
            StreamState::UnAuthenticated(auth) => {
                let _ = auth.send(true);
            }
        };
    }

    fn poll_authenticated(&mut self) -> Poll<Option<P>, Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(None)),
            };

            match msg {
                Protocol::Embedded(msg) => return Ok(Ready(Some(msg))),
                Protocol::RequestPeerConnection(connection_id, msg, mut addresses) => {
                    addresses.push(self.stream.get_ref().get_ref().get_ref().peer_addr());
                    self.incoming_con_requests.insert(connection_id, addresses);
                    return Ok(Ready(Some(msg)));
                }
                Protocol::RequestPrivateAdressInformation => {
                    let addresses = get_interface_addresses(
                        self.stream.get_ref().get_ref().get_ref().local_addr(),
                    );
                    self.direct_send(Protocol::PrivateAdressInformation(addresses))?;
                }
                Protocol::PrivateAdressInformation(mut addresses) => {
                    addresses.push(self.stream.get_ref().get_ref().get_ref().peer_addr());

                    for req in self.address_info_requests.drain(..).into_iter() {
                        req.add_address_info(addresses.clone());
                    }
                }
                Protocol::Connect(addresses, _, connection_id) => {
                    self.connect_callback.unbounded_send((
                        addresses,
                        connection_id,
                        self.stream_handle.clone(),
                    ));
                }
                Protocol::RequestRelayPeerConnection(connection_id) => {
                    if let Some(req) = self.con_requests.remove(&connection_id) {
                        req.relay_connection();
                    }
                }
                _ => {}
            };
        }
    }

    fn poll_unauthenticated(&mut self) -> Poll<Option<P>, Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(None)),
            };

            match msg {
                Protocol::Embedded(msg) => return Ok(Ready(Some(msg))),
                _ => {}
            };
        }
    }

    /// INTERNAL USE ONLY
    /// Can be used to poll the underlying `strategies::Stream` directly. This enables handlers to
    /// to process protocol messages.
    pub(crate) fn direct_poll(&mut self) -> Poll<Option<Protocol<P>>, Error> {
        self.stream.poll().map_err(|e| e.into())
    }

    pub(crate) fn direct_send(&mut self, item: Protocol<P>) -> Result<()> {
        if self.stream.start_send(item).is_err() || self.stream.poll_complete().is_err() {
            bail!("error at `direct_send`");
        }

        Ok(())
    }

    pub fn create_connection_to(
        &mut self,
        connection_id: ConnectionId,
        other: &mut StreamHandle<P>,
    ) -> Result<()> {
        let addresses = match self.incoming_con_requests.remove(&connection_id) {
            Some(addresses) => addresses,
            None => bail!("unknown connection id"),
        };

        let (master, slave) = connection_request::ConnectionRequest::new(
            connection_id,
            &self.handle,
            self.stream_handle.clone(),
            other.clone(),
            addresses,
        );

        self.con_requests.insert(connection_id, master);
        other.register_address_info_handle(slave);

        Ok(())
    }

    pub fn get_stream_handle(&self) -> StreamHandle<P> {
        self.stream_handle.clone()
    }

    pub fn send_and_poll(&mut self, item: P) -> Result<()> {
        self.direct_send(Protocol::Embedded(item))
    }

    pub fn into_plain(self) -> strategies::Stream {
        self.stream.into_inner().into_inner().into_inner()
    }

    pub fn is_p2p(&self) -> bool {
        self.is_p2p_con
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.stream.get_ref().get_ref().get_ref().local_addr()
    }
}

impl<P> GetConnectionId for Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn connection_id(&self) -> ConnectionId {
        self.stream.get_ref().get_ref().get_ref().connection_id()
    }
}

impl<P> FStream for Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = P;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match self.stream_handle_recv.poll() {
                Ok(Ready(Some(msg))) => match msg {
                    HandleProtocol::Send(msg) => {
                        self.direct_send(msg)?;
                    }
                    HandleProtocol::AddressInfoRequest(req) => {
                        self.address_info_requests.push(req);
                    }
                },
                _ => break,
            }
        }

        match self.state {
            StreamState::Authenticated => self.poll_authenticated(),
            StreamState::UnAuthenticated(_) => self.poll_unauthenticated(),
        }
    }
}

impl<P> Sink for Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type SinkItem = P;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.stream
            .start_send(Protocol::Embedded(item))
            .map(|r| {
                r.map(|v| match v {
                    Protocol::Embedded(item) => item,
                    _ => unreachable!(),
                })
            })
            .map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete().map_err(|e| e.into())
    }
}

#[derive(Clone)]
pub struct StreamHandle<P> {
    send: mpsc::UnboundedSender<HandleProtocol<P>>,
    new_stream_handle: NewStreamHandle<P>,
}

impl<P> StreamHandle<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        send: mpsc::UnboundedSender<HandleProtocol<P>>,
        new_stream_handle: NewStreamHandle<P>,
    ) -> StreamHandle<P> {
        StreamHandle {
            send,
            new_stream_handle,
        }
    }

    pub(crate) fn send_msg(&mut self, msg: Protocol<P>) {
        let _ = self.send.unbounded_send(HandleProtocol::Send(msg));
    }

    pub(crate) fn register_address_info_handle(
        &mut self,
        handle: connection_request::ConnectionRequestSlaveHandle,
    ) {
        let _ = self
            .send
            .unbounded_send(HandleProtocol::AddressInfoRequest(handle));
    }

    pub(crate) fn new_stream(&mut self) -> NewStreamFuture<P> {
        self.new_stream_handle.new_stream()
    }
}
