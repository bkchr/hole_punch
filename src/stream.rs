use connection::{ConnectPeers, ConnectionId};
use connection_request;
use context::{PassStreamToContext, ResolvePeer, ResolvePeerResult};
use error::*;
use protocol::{BuildPeerConnection, Protocol};
use strategies::{self, AddressInformation, GetConnectionId, NewStream};

use std::collections::HashMap;
use std::mem;
use std::net::SocketAddr;

use futures::sync::{mpsc, oneshot};
use futures::Async::Ready;
use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use tokio_io::codec::length_delimited;

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use itertools::Itertools;

#[derive(Clone)]
pub struct NewStreamHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_stream_handle: strategies::NewStreamHandle,
    pass_stream_to_context: PassStreamToContext<P>,
    resolve_peer: R,
    connect_peers: ConnectPeers<P>,
    is_p2p_con: bool,
    handle: Handle,
}

impl<P, R> NewStreamHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        new_stream_handle: strategies::NewStreamHandle,
        pass_stream_to_context: PassStreamToContext<P>,
        resolve_peer: R,
        connect_peers: ConnectPeers<P>,
        is_p2p_con: bool,
        handle: &Handle,
    ) -> NewStreamHandle<P, R> {
        NewStreamHandle {
            new_stream_handle,
            pass_stream_to_context,
            resolve_peer,
            connect_peers,
            is_p2p_con,
            handle: handle.clone(),
        }
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P, R> {
        NewStreamFuture::new(
            self.new_stream_handle.new_stream(),
            self.clone(),
            self.pass_stream_to_context.clone(),
            self.resolve_peer.clone(),
            self.connect_peers.clone(),
            self.is_p2p_con,
            &self.handle,
        )
    }
}

pub struct NewStreamFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_stream: strategies::NewStreamFuture,
    new_stream_handle: NewStreamHandle<P, R>,
    pass_stream_to_context: PassStreamToContext<P>,
    resolve_peer: R,
    connect_peers: ConnectPeers<P>,
    handle: Handle,
    is_p2p_con: bool,
}

impl<P, R> NewStreamFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        new_stream: strategies::NewStreamFuture,
        new_stream_handle: NewStreamHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P>,
        resolve_peer: R,
        connect_peers: ConnectPeers<P>,
        is_p2p_con: bool,
        handle: &Handle,
    ) -> NewStreamFuture<P, R> {
        NewStreamFuture {
            new_stream,
            new_stream_handle,
            pass_stream_to_context,
            resolve_peer,
            connect_peers,
            is_p2p_con,
            handle: handle.clone(),
        }
    }
}

impl<P, R> Future for NewStreamFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_stream.poll().map(|r| {
            r.map(|v| {
                Stream::new(
                    v,
                    None,
                    &self.handle,
                    self.new_stream_handle.clone(),
                    self.pass_stream_to_context.clone(),
                    self.resolve_peer.clone(),
                    self.connect_peers.clone(),
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
    UnAuthenticated(oneshot::Sender<()>),
}

enum HandleProtocol<P, R> {
    Send(Protocol<P, R>),
    AddressInfoRequest(connection_request::ConnectionRequestSlaveHandle),
}

pub struct Stream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    stream: WriteJson<
        ReadJson<length_delimited::Framed<strategies::Stream>, Protocol<P, R>>,
        Protocol<P, R>,
    >,
    state: StreamState,
    stream_handle: StreamHandle<P, R>,
    stream_handle_recv: mpsc::UnboundedReceiver<HandleProtocol<P, R>>,
    con_requests: HashMap<ConnectionId, connection_request::ConnectionRequestMasterHandle>,
    incoming_con_requests: HashMap<ConnectionId, Vec<SocketAddr>>,
    address_info_requests: Vec<connection_request::ConnectionRequestSlaveHandle>,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P>,
    resolve_peer: R,
    connect_peers: ConnectPeers<P>,
    is_p2p_con: bool,
    requested_connections: HashMap<ConnectionId, oneshot::Sender<Stream<P, R>>>,
}

impl<P, R> Stream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        stream: strategies::Stream,
        auth_con: Option<oneshot::Sender<()>>,
        handle: &Handle,
        new_stream_handle: NewStreamHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P>,
        resolve_peer: R,
        connect_peers: ConnectPeers<P>,
        is_p2p_con: bool,
    ) -> Stream<P, R> {
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
            pass_stream_to_context,
            resolve_peer,
            connect_peers,
            stream_handle,
            stream_handle_recv,
            is_p2p_con,
            requested_connections: HashMap::new(),
        }
    }

    /// Upgrades this `Stream` and the underlying `Connection` to be authenticated.
    /// This allows the `Connection` to accept multiple `Stream`s and the `Stream`s can execute
    /// the full protocol, e.g. opening a connection to a different device.
    pub fn upgrade_to_authenticated(&mut self) {
        match mem::replace(&mut self.state, StreamState::Authenticated) {
            StreamState::Authenticated => {}
            StreamState::UnAuthenticated(auth) => {
                let _ = auth.send(());
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
                Protocol::BuildPeerConnection(id, info) => {
                    match info {
                        BuildPeerConnection::RequestPeer(peer, mut addresses) => {
                            match self.resolve_peer.resolve_peer(peer) {
                                ResolvePeerResult::Found(handle) => {
                                    addresses.push(
                                        self.stream.get_ref().get_ref().get_ref().peer_addr(),
                                    );
                                }
                                ResolvePeerResult::NotFound => {
                                    self.send_and_poll(Protocol::BuildPeerConnection(
                                        id,
                                        BuildPeerConnection::NotFound,
                                    ))?;
                                }
                                ResolvePeerResult::NotFoundLocally(addr) => {
                                    self.send_and_poll(Protocol::BuildPeerConnection(
                                        id,
                                        BuildPeerConnection::PeerNotFoundLocally(addr),
                                    ))?;
                                }
                            }
                        }
                        BuildPeerConnection::PeerNotFound => {}
                        BuildPeerConnection::PeerNotFoundLocally(_) => {}
                        BuildPeerConnection::ConnectToPeer(addresses) => {
                            self.connect_peers
                                .connect(addresses, id, self.stream_handle.clone());
                        }
                    };
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
    pub(crate) fn direct_poll(&mut self) -> Poll<Option<Protocol<P, R>>, Error> {
        self.stream.poll().map_err(|e| e.into())
    }

    pub(crate) fn direct_send(&mut self, item: Protocol<P, R>) -> Result<()> {
        if self.stream.start_send(item).is_err() || self.stream.poll_complete().is_err() {
            bail!("error at `direct_send`");
        }

        Ok(())
    }

    pub fn request_connection_to_peer(
        &mut self,
        connection_id: ConnectionId,
        server: &mut Stream<P, R>,
        peer: R::Identifier,
    ) -> Result<NewPeerConnection<P, R>> {
        if self.requested_connections.contains_key(&connection_id) {
            bail!("connection with the same id was already requested");
        }

        let addresses = get_interface_addresses(server.local_addr());
        server.direct_send(Protocol::RequestPeerConnection(
            connection_id,
            peer,
            addresses,
        ))?;

        let (sender, receiver) = oneshot::channel();

        self.requested_connections.insert(connection_id, sender);
        Ok(NewPeerConnection::new(receiver))
    }

    pub fn get_stream_handle(&self) -> StreamHandle<P, R> {
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

impl<P, R> GetConnectionId for Stream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn connection_id(&self) -> ConnectionId {
        self.stream.get_ref().get_ref().get_ref().connection_id()
    }
}

impl<P, R> FStream for Stream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
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

impl<P, R> Sink for Stream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
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
pub struct StreamHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    send: mpsc::UnboundedSender<HandleProtocol<P, R>>,
    new_stream_handle: NewStreamHandle<P, R>,
}

impl<P, R> StreamHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn new(
        send: mpsc::UnboundedSender<HandleProtocol<P, R>>,
        new_stream_handle: NewStreamHandle<P, R>,
    ) -> StreamHandle<P, R> {
        StreamHandle {
            send,
            new_stream_handle,
        }
    }

    pub(crate) fn send_msg(&mut self, msg: Protocol<P, R>) {
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

    pub(crate) fn new_stream(&mut self) -> NewStreamFuture<P, R> {
        self.new_stream_handle.new_stream()
    }
}

pub struct NewPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    recv: oneshot::Receiver<Stream<P, R>>,
}

impl<P, R> NewPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn new(recv: oneshot::Receiver<Stream<P, R>>) -> NewPeerConnection<P, R> {
        NewPeerConnection { recv }
    }
}

impl<P, R> Future for NewPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.recv.poll().map_err(|e| e.into())
    }
}
