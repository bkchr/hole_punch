use connect::build_connection_to_peer;
use connection::{ConnectionId, NewConnectionHandle};
use context::{PassStreamToContext, ResolvePeer, ResolvePeerResult};
use error::*;
use protocol::{BuildPeerToPeerConnection, LocatePeer, Protocol};
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
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
    new_con_handle: NewConnectionHandle<P, R>,
    handle: Handle,
}

impl<P, R> NewStreamHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        new_stream_handle: strategies::NewStreamHandle,
        new_con_handle: NewConnectionHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> NewStreamHandle<P, R> {
        NewStreamHandle {
            new_stream_handle,
            new_con_handle,
            pass_stream_to_context,
            resolve_peer,
            handle: handle.clone(),
        }
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P, R> {
        NewStreamFuture::new(
            self.new_stream_handle.new_stream(),
            self.clone(),
            self.new_con_handle.clone(),
            self.pass_stream_to_context.clone(),
            self.resolve_peer.clone(),
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
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
    new_con_handle: NewConnectionHandle<P, R>,
    handle: Handle,
}

impl<P, R> NewStreamFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        new_stream: strategies::NewStreamFuture,
        new_stream_handle: NewStreamHandle<P, R>,
        new_con_handle: NewConnectionHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> NewStreamFuture<P, R> {
        NewStreamFuture {
            new_stream,
            new_stream_handle,
            pass_stream_to_context,
            resolve_peer,
            new_con_handle,
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
                    self.new_con_handle.clone(),
                    self.pass_stream_to_context.clone(),
                    self.resolve_peer.clone(),
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

enum HandleProtocol<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    Send(Protocol<P, R>),
    AddressInformationRequest(R::Identifier, StreamHandle<P, R>),
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
    address_info_requests: Vec<(R::Identifier, StreamHandle<P, R>)>,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
    new_con_handle: NewConnectionHandle<P, R>,
    is_p2p_con: bool,
    requested_connections: HashMap<R::Identifier, oneshot::Sender<Stream<P, R>>>,
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
        new_con_handle: NewConnectionHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
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
            address_info_requests: Vec::new(),
            handle: handle.clone(),
            pass_stream_to_context,
            resolve_peer,
            new_con_handle,
            stream_handle,
            stream_handle_recv,
            is_p2p_con: true,
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
                Protocol::LocatePeer(info) => {
                    match info {
                        LocatePeer::Locate(peer) => match self.resolve_peer.resolve_peer(&peer) {
                            ResolvePeerResult::FoundLocally(_) => {
                                self.send_and_poll(LocatePeer::FoundLocally(peer))?;
                            }
                            ResolvePeerResult::NotFound => {
                                self.send_and_poll(LocatePeer::NotFound(peer))?;
                            }
                            ResolvePeerResult::FoundRemote(addr) => {
                                self.send_and_poll(LocatePeer::FoundRemote(peer, addr))?;
                            }
                        },
                        LocatePeer::NotFound(peer) => {
                            self.requested_connections.remove(&peer);
                        }
                        LocatePeer::FoundRemote(_, _) => unimplemented!(),
                        LocatePeer::FoundLocally(peer) => {
                            if self.requested_connections.contains_key(&peer) {
                                let addresses = get_interface_addresses(
                                    self.stream.get_ref().get_ref().get_ref().local_addr(),
                                );
                                self.send_and_poll(
                                    BuildPeerToPeerConnection::AddressInformationExchange(
                                        peer,
                                        addresses,
                                    ),
                                )?;
                            }
                        }
                    };
                }
                Protocol::BuildPeerToPeerConnection(op) => match op {
                    BuildPeerToPeerConnection::AddressInformationExchange(peer, mut addresses) => {
                        match self.resolve_peer.resolve_peer(&peer) {
                            ResolvePeerResult::FoundLocally(mut handle) => {
                                handle.request_address_information(
                                    peer.clone(),
                                    self.stream_handle.clone(),
                                );

                                addresses
                                    .push(self.stream.get_ref().get_ref().get_ref().peer_addr());
                                handle.send_msg(BuildPeerToPeerConnection::ConnectionCreate(
                                    peer, addresses,
                                ));
                            }
                            _ => {
                                self.send_and_poll(BuildPeerToPeerConnection::PeerNotFound(peer))?;
                            }
                        }
                    }
                    BuildPeerToPeerConnection::AddressInformationRequest => {
                        let addresses = get_interface_addresses(
                            self.stream.get_ref().get_ref().get_ref().local_addr(),
                        );
                        self.send_and_poll(BuildPeerToPeerConnection::AddressInformationResponse(
                            addresses,
                        ))?;
                    }
                    BuildPeerToPeerConnection::AddressInformationResponse(mut addresses) => {
                        addresses.push(self.stream.get_ref().get_ref().get_ref().peer_addr());
                        self.address_info_requests
                            .drain(..)
                            .for_each(|(peer, mut handle)| {
                                handle.send_msg(BuildPeerToPeerConnection::ConnectionCreate(
                                    peer,
                                    addresses.clone(),
                                ))
                            });
                    }
                    BuildPeerToPeerConnection::ConnectionCreate(peer, addresses) => {
                        let sender = self.requested_connections.remove(&peer);
                        build_connection_to_peer(
                            self.new_con_handle.clone(),
                            peer,
                            addresses,
                            self.get_stream_handle(),
                            &self.handle,
                            sender,
                        );
                    }
                    BuildPeerToPeerConnection::PeerNotFound(peer) => {}
                },
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

    pub(crate) fn set_p2p(&mut self, p2p: bool) {
        self.is_p2p_con = p2p;
    }

    pub fn request_connection_to_peer(
        &mut self,
        server: &mut Stream<P, R>,
        peer: R::Identifier,
    ) -> Result<NewPeerConnection<P, R>> {
        server.send_and_poll(LocatePeer::Locate(peer.clone()))?;

        let (sender, receiver) = oneshot::channel();

        self.requested_connections.insert(peer, sender);
        Ok(NewPeerConnection::new(receiver))
    }

    pub fn get_stream_handle(&self) -> StreamHandle<P, R> {
        self.stream_handle.clone()
    }

    pub fn send_and_poll<T: Into<Protocol<P, R>>>(&mut self, item: T) -> Result<()> {
        self.direct_send(item.into())
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
                        self.send_and_poll(msg)?;
                    }
                    HandleProtocol::AddressInformationRequest(peer, handle) => {
                        self.address_info_requests.push((peer, handle));
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

    pub(crate) fn send_msg<T: Into<Protocol<P, R>>>(&mut self, msg: T) {
        let _ = self.send.unbounded_send(HandleProtocol::Send(msg.into()));
    }

    pub(crate) fn request_address_information(&mut self, peer: R::Identifier, handle: Self) {
        let _ = self
            .send
            .unbounded_send(HandleProtocol::AddressInformationRequest(peer, handle));
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
