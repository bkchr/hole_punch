use error::*;
use strategies::{self, AddressInformation, GetConnectionId, NewConnection, NewStream};
use config::Config;
use incoming;
use protocol::Protocol;
use connect::{self, ConnectWithStrategies};
use connection_request;
use authenticator::Authenticator;

use std::time::Duration;
use std::net::SocketAddr;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::{Future, Poll, Sink, StartSend, Stream as FStream};
use futures::Async::{NotReady, Ready};
use futures::sync::{mpsc, oneshot};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use tokio_io::codec::length_delimited;

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use rand::{self, Rng};

use itertools::Itertools;

pub type ConnectionId = u64;

struct GetMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    stream: Option<Stream<P>>,
}

impl<P> GetMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(stream: Stream<P>) -> GetMessage<P> {
        GetMessage {
            stream: Some(stream),
        }
    }
}

impl<P> Future for GetMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (Stream<P>, Protocol<P>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let msg = match try_ready!(
            self.stream
                .as_mut()
                .expect("can not be polled twice")
                .direct_poll()
        ) {
            Some(msg) => msg,
            None => bail!("returned None"),
        };

        Ok(Ready((self.stream.take().unwrap(), msg)))
    }
}

pub struct Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    incoming: FuturesUnordered<incoming::HandlerFuture<P>>,
    requested_connections: HashMap<ConnectionId, oneshot::Sender<Stream<P>>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle<P>>,
    device_to_device_callback: (
        mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        mpsc::UnboundedReceiver<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    ),
    outgoing_device_con: FuturesUnordered<connect::DeviceToDeviceConnectionFuture<P>>,
    connections: FuturesUnordered<StreamFuture<Connection<P>>>,
    new_connection: (
        mpsc::UnboundedSender<Connection<P>>,
        mpsc::UnboundedReceiver<Connection<P>>,
    ),
    incoming_stream: FuturesUnordered<GetMessage<P>>,
    authenticator: Option<Authenticator>,
}

impl<P> Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(handle: Handle, config: Config) -> Result<Context<P>> {
        let authenticator =
            if config.client_ca_certificates.is_some() || config.server_ca_certificates.is_some() {
                Some(Authenticator::new(
                    config.server_ca_certificates.as_ref().cloned(),
                    config.client_ca_certificates.as_ref().cloned(),
                )?)
            } else {
                None
            };

        let strats = strategies::init(handle.clone(), &config, authenticator.as_ref())?;

        let device_to_device_callback = mpsc::unbounded();

        let new_connection_handles = strats
            .iter()
            .map(|s| {
                NewConnectionHandle::new(
                    s.get_new_connection_handle(),
                    device_to_device_callback.0.clone(),
                    &handle,
                )
            })
            .collect();

        Ok(Context {
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
            incoming: FuturesUnordered::new(),
            handle,
            new_connection_handles,
            requested_connections: HashMap::new(),
            device_to_device_callback,
            outgoing_device_con: FuturesUnordered::new(),
            connections: FuturesUnordered::new(),
            new_connection: mpsc::unbounded(),
            incoming_stream: FuturesUnordered::new(),
            authenticator,
        })
    }

    /// Returns the `Authenticator`.
    /// The authenticator is only created, if the `Config` contained trusted client/server
    /// certificates.
    pub fn authenticator(&self) -> Option<Authenticator> {
        self.authenticator.as_ref().cloned()
    }

    fn poll_strategies(&mut self) -> Result<()> {
        loop {
            match self.strategies.poll() {
                Ok(NotReady) => return Ok(()),
                Err(e) => return Err(e.0),
                Ok(Ready(Some((Some(con), strat)))) => {
                    self.incoming.push(incoming::Handler::new(
                        Connection::new(
                            con,
                            self.device_to_device_callback.0.clone(),
                            &self.handle,
                        ),
                        Duration::from_secs(1),
                        &self.handle,
                    ));
                    self.strategies.push(strat.into_future());
                }
                Ok(Ready(Some((None, _)))) => {
                    bail!("strategy returned None!");
                }
                Ok(Ready(None)) => {
                    panic!("strategies empty");
                }
            }
        }
    }

    fn poll_device_to_device_callback(&mut self) {
        loop {
            match self.device_to_device_callback.1.poll() {
                Ok(Ready(Some((addresses, connection_id, stream_handle)))) => {
                    self.outgoing_device_con
                        .push(connect::DeviceToDeviceConnection::start(
                            self.new_connection_handles.get(0).unwrap().clone(),
                            addresses,
                            stream_handle,
                            connection_id,
                            self.requested_connections.contains_key(&connection_id),
                            self.handle.clone(),
                        ));
                }
                _ => {
                    break;
                }
            }
        }
    }

    fn poll_outgoing_device_connections(&mut self) -> Poll<Option<Stream<P>>, Error> {
        loop {
            let (con, stream, id) = match try_ready!(self.outgoing_device_con.poll()) {
                Some(res) => res,
                None => return Ok(NotReady),
            };

            if let Some(con) = con {
                self.connections.push(con.into_future());
            }

            if let Some(stream) = stream {
                match self.requested_connections.remove(&id) {
                    Some(cb) => {
                        cb.send(stream);
                    }
                    None => return Ok(Ready(Some(stream))),
                }
            }
        }
    }

    fn poll_new_connection(&mut self) {
        loop {
            match self.new_connection.1.poll() {
                Ok(Ready(Some(con))) => {
                    self.connections.push(con.into_future());
                }
                _ => {
                    return;
                }
            }
        }
    }

    pub fn generate_connection_id(&self) -> ConnectionId {
        let mut rng = rand::thread_rng();

        loop {
            let id = rng.next_u64();

            if !self.requested_connections.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn create_connection_to_peer(
        &mut self,
        connection_id: ConnectionId,
        server: &mut Stream<P>,
        msg: P,
    ) -> Result<NewPeerConnection<P>> {
        if self.requested_connections.contains_key(&connection_id) {
            bail!("connection with the same id was already requested");
        }

        let addresses =
            get_interface_addresses(server.stream.get_ref().get_ref().get_ref().local_addr());
        server.direct_send(Protocol::RequestPeerConnection(
            connection_id,
            msg,
            addresses,
        ))?;

        let (sender, receiver) = oneshot::channel();

        self.requested_connections.insert(connection_id, sender);
        Ok(NewPeerConnection::new(receiver))
    }

    pub fn create_connection_to_server(&mut self, addr: &SocketAddr) -> NewConnectionToServer<P> {
        NewConnectionToServer::new(
            ConnectWithStrategies::new(self.new_connection_handles.clone(), &self.handle, *addr),
            self.new_connection.0.clone(),
        )
    }
}

pub struct NewConnectionToServer<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    connect: ConnectWithStrategies<P>,
    new_con_send: mpsc::UnboundedSender<Connection<P>>,
}

impl<P> NewConnectionToServer<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        connect: ConnectWithStrategies<P>,
        new_con_send: mpsc::UnboundedSender<Connection<P>>,
    ) -> NewConnectionToServer<P> {
        NewConnectionToServer {
            connect,
            new_con_send,
        }
    }
}

impl<P> Future for NewConnectionToServer<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (con, stream) = try_ready!(self.connect.poll());

        let _ = self.new_con_send.unbounded_send(con);

        Ok(Ready(stream))
    }
}

pub struct NewPeerConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    recv: oneshot::Receiver<Stream<P>>,
}

impl<P> NewPeerConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(recv: oneshot::Receiver<Stream<P>>) -> NewPeerConnection<P> {
        NewPeerConnection { recv }
    }
}

impl<P> Future for NewPeerConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.recv.poll().map_err(|e| e.into())
    }
}

impl<P> FStream for Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;
        self.poll_device_to_device_callback();
        self.poll_new_connection();

        match self.poll_outgoing_device_connections() {
            Ok(Ready(Some(res))) => return Ok(Ready(Some(res))),
            _ => {}
        };

        loop {
            match self.connections.poll() {
                Ok(Ready(Some((stream, con)))) => match stream {
                    Some(stream) => {
                        self.connections.push(con.into_future());
                        self.incoming_stream.push(GetMessage::new(stream));
                    }
                    _ => {}
                },
                _ => {
                    break;
                }
            }
        }

        loop {
            match self.incoming_stream.poll() {
                Ok(Ready(Some((stream, msg)))) => match msg {
                    Protocol::RelayConnection(id) => match self.requested_connections.remove(&id) {
                        Some(cb) => {
                            cb.send(stream);
                        }
                        None => {}
                    },
                    _ => unimplemented!(),
                },
                _ => {
                    break;
                }
            }
        }

        loop {
            match try_ready!(self.incoming.poll()) {
                Some(Some((con, stream, _))) => {
                    self.connections.push(con.into_future());
                    return Ok(Ready(Some(stream)));
                }
                // If the incoming handler returns `None`, then the incoming connection
                // does not need to be propagated.
                Some(None) => {}
                None => {
                    return Ok(NotReady);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct NewConnectionHandle<P> {
    new_con: strategies::NewConnectionHandle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    handle: Handle,
}

impl<P> NewConnectionHandle<P> {
    fn new(
        new_con: strategies::NewConnectionHandle,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> NewConnectionHandle<P> {
        NewConnectionHandle {
            new_con,
            connect_callback,
            handle: handle.clone(),
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture<P> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.connect_callback.clone(),
            &self.handle,
        )
    }
}

pub struct NewConnectionFuture<P> {
    new_con_recv: strategies::NewConnectionFuture,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    _marker: PhantomData<P>,
    handle: Handle,
}

impl<P> NewConnectionFuture<P> {
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> NewConnectionFuture<P> {
        NewConnectionFuture {
            new_con_recv,
            connect_callback,
            _marker: Default::default(),
            handle: handle.clone(),
        }
    }
}

impl<P> Future for NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_con_recv
            .poll()
            .map(|r| r.map(|v| Connection::new(v, self.connect_callback.clone(), &self.handle)))
    }
}

enum ConnectionState {
    UnAuthenticated {
        auth_recv: oneshot::Receiver<bool>,
        auth_send: Option<oneshot::Sender<bool>>,
    },
    Authenticated,
}

pub struct Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    con: strategies::Connection,
    state: ConnectionState,
    handle: Handle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    is_p2p: bool,
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        con: strategies::Connection,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> Connection<P> {
        let (send, auth_recv) = oneshot::channel();

        Connection {
            con,
            state: ConnectionState::UnAuthenticated {
                auth_recv,
                auth_send: Some(send),
            },
            handle: handle.clone(),
            connect_callback,
            is_p2p: false,
        }
    }

    pub fn set_p2p(&mut self, p2p: bool) {
        self.is_p2p = p2p;
    }

    pub fn is_p2p(&self) -> bool {
        self.is_p2p
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.con.new_stream(),
            self.get_new_stream_handle(),
            self.connect_callback.clone(),
            self.is_p2p,
            &self.handle,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle<P> {
        NewStreamHandle::new(
            self.con.get_new_stream_handle(),
            self.connect_callback.clone(),
            self.is_p2p,
            &self.handle,
        )
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.con.peer_addr()
    }
}

impl<P> FStream for Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let state = match self.state {
                ConnectionState::Authenticated => {
                    return self.con.poll().map(|r| {
                        r.map(|o| {
                            o.map(|v| {
                                Stream::new(
                                    v,
                                    None,
                                    &self.handle,
                                    self.get_new_stream_handle(),
                                    self.connect_callback.clone(),
                                    self.is_p2p,
                                )
                            })
                        })
                    })
                }
                ConnectionState::UnAuthenticated {
                    ref mut auth_recv,
                    ref mut auth_send,
                } => {
                    loop {
                        let auth = match auth_recv.poll() {
                            Ok(Ready(auth)) => {
                                assert!(auth);
                                auth
                            }
                            _ => false,
                        };

                        if auth {
                            break;
                        } else {
                            let stream = match try_ready!(self.con.poll()) {
                                Some(stream) => stream,
                                None => return Ok(Ready(None)),
                            };

                            // Take `auth_send` and return the new `Stream`.
                            // If `auth_send` is None, we don't propagate any longer `Stream`s,
                            // because only one `Stream` is allowed for unauthorized `Connection`s.
                            if let Some(send) = auth_send.take() {
                                return Ok(Ready(Some(Stream::new(
                                    stream,
                                    Some(send),
                                    &self.handle,
                                    NewStreamHandle::new(
                                        self.con.get_new_stream_handle(),
                                        self.connect_callback.clone(),
                                        self.is_p2p,
                                        &self.handle,
                                    ),
                                    self.connect_callback.clone(),
                                    self.is_p2p,
                                ))));
                            }
                        }
                    }

                    ConnectionState::Authenticated
                }
            };

            self.state = state;
        }
    }
}

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
    fn new(
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
    fn new(
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

fn get_interface_addresses(local_addr: SocketAddr) -> Vec<SocketAddr> {
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
    fn new(
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
                    self.direct_send(Protocol::PrivateAdressInformation(addresses));
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
                        self.direct_send(msg);
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
        let _ = self.send
            .unbounded_send(HandleProtocol::AddressInfoRequest(handle));
    }

    pub(crate) fn new_stream(&mut self) -> NewStreamFuture<P> {
        self.new_stream_handle.new_stream()
    }
}
