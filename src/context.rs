use errors::*;
use strategies::{self, AddressInformation, NewConnection, NewStream};
use config::Config;
use incoming;
use protocol::Protocol;
use connect::{ConnectToPeerCoordinator, ConnectToPeerHandle, ConnectWithStrategies};

use std::time::Duration;
use std::net::SocketAddr;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::mem;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::{Future, Poll, Sink, StartSend, Stream as FStream};
use futures::Async::{NotReady, Ready};
use futures::sync::{mpsc, oneshot};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

use either::Either;

use pnet_datalink::interfaces;

use rand::{self, Rng};

use itertools::Itertools;

pub type ConnectionId = u64;

pub struct Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    incoming: FuturesUnordered<incoming::HandlerFuture<P>>,
    requested_connections:
        HashMap<ConnectionId, oneshot::Sender<Either<(Connection<P>, Stream<P>), Stream<P>>>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle>,
}

impl<P> Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(handle: Handle, config: Config) -> Result<Context<P>> {
        let strats = strategies::init(handle.clone(), &config)
            .chain_err(|| "error initializing the strategies")?;

        let new_connection_handles = strats
            .iter()
            .map(|s| NewConnectionHandle::new(s.get_new_connection_handle(), &handle))
            .collect();

        Ok(Context {
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
            incoming: FuturesUnordered::new(),
            handle,
            new_connection_handles,
            requested_connections: HashMap::new(),
        })
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
                            NewConnectionHandle::new(
                                strat.get_new_connection_handle(),
                                &self.handle,
                            ),
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

        server.direct_send(Protocol::RequestPeerConnection(connection_id, msg))?;

        let (sender, receiver) = oneshot::channel();

        self.requested_connections.insert(connection_id, sender);
        Ok(NewPeerConnection::new(receiver))
    }

    pub fn create_connection_to_server(&mut self, addr: &SocketAddr) -> ConnectWithStrategies<P> {
        ConnectWithStrategies::new(self.new_connection_handles.clone(), &self.handle, *addr)
    }
}

pub struct NewPeerConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    recv: oneshot::Receiver<Either<(Connection<P>, Stream<P>), Stream<P>>>,
}

impl<P> NewPeerConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        recv: oneshot::Receiver<Either<(Connection<P>, Stream<P>), Stream<P>>>,
    ) -> NewPeerConnection<P> {
        NewPeerConnection { recv }
    }
}

impl<P> FStream for Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (Connection<P>, Stream<P>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;

        loop {
            match try_ready!(self.incoming.poll()) {
                Some(Some((con, stream, _))) => return Ok(Ready(Some((con, stream)))),
                // If the incoming handler returns `None`, then the incoming connection
                // does not need to be propagated.
                _ => {}
            }
        }
    }
}

#[derive(Clone)]
pub struct NewConnectionHandle {
    new_con: strategies::NewConnectionHandle,
    handle: Handle,
}

impl NewConnectionHandle {
    fn new(new_con: strategies::NewConnectionHandle, handle: &Handle) -> NewConnectionHandle {
        NewConnectionHandle {
            new_con,
            handle: handle.clone(),
        }
    }

    pub fn new_connection<P>(&mut self, addr: SocketAddr) -> NewConnectionFuture<P> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.clone(),
            &self.handle,
        )
    }
}

pub struct NewConnectionFuture<P> {
    new_con_recv: strategies::NewConnectionFuture,
    new_con_handle: NewConnectionHandle,
    _marker: PhantomData<P>,
    handle: Handle,
}

impl<P> NewConnectionFuture<P> {
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        new_con_handle: NewConnectionHandle,
        handle: &Handle,
    ) -> NewConnectionFuture<P> {
        NewConnectionFuture {
            new_con_recv,
            new_con_handle,
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
            .map(|r| r.map(|v| Connection::new(v, self.new_con_handle.clone(), &self.handle)))
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
    new_con_handle: NewConnectionHandle,
    _marker: PhantomData<P>,
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        con: strategies::Connection,
        new_con_handle: NewConnectionHandle,
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
            _marker: Default::default(),
            new_con_handle,
        }
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.con.new_stream(),
            self.get_new_stream_handle(),
            self.new_con_handle.clone(),
            &self.handle,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(
            self.con.get_new_stream_handle(),
            self.new_con_handle.clone(),
            &self.handle,
        )
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
                                    self.new_con_handle.clone(),
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
                                        self.new_con_handle.clone(),
                                        &self.handle,
                                    ),
                                    self.new_con_handle.clone(),
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
pub struct NewStreamHandle {
    new_stream_handle: strategies::NewStreamHandle,
    new_con_handle: NewConnectionHandle,
    handle: Handle,
}

impl NewStreamHandle {
    fn new(
        new_stream_handle: strategies::NewStreamHandle,
        new_con_handle: NewConnectionHandle,
        handle: &Handle,
    ) -> NewStreamHandle {
        NewStreamHandle {
            new_stream_handle,
            new_con_handle,
            handle: handle.clone(),
        }
    }

    pub fn new_stream<P>(&mut self) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.new_stream_handle.new_stream(),
            self.clone(),
            self.new_con_handle.clone(),
            &self.handle,
        )
    }
}

pub struct NewStreamFuture<P> {
    new_stream: strategies::NewStreamFuture,
    new_stream_handle: NewStreamHandle,
    new_con_handle: NewConnectionHandle,
    handle: Handle,
    _marker: PhantomData<P>,
}

impl<P> NewStreamFuture<P> {
    fn new(
        new_stream: strategies::NewStreamFuture,
        new_stream_handle: NewStreamHandle,
        new_con_handle: NewConnectionHandle,
        handle: &Handle,
    ) -> NewStreamFuture<P> {
        NewStreamFuture {
            new_stream,
            new_stream_handle,
            new_con_handle,
            handle: handle.clone(),
            _marker: Default::default(),
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
                    self.new_con_handle.clone(),
                )
            })
        })
    }
}

enum StreamState {
    Authenticated,
    UnAuthenticated(oneshot::Sender<bool>),
}

pub struct Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    stream: WriteJson<ReadJson<strategies::Stream, Protocol<P>>, Protocol<P>>,
    state: StreamState,
    remote_msg: (
        mpsc::UnboundedSender<Protocol<P>>,
        mpsc::UnboundedReceiver<Protocol<P>>,
    ),
    con_requests: HashMap<ConnectionId, ConnectToPeerHandle>,
    incoming_con_requests: HashSet<ConnectionId>,
    handle: Handle,
    new_stream_handle: NewStreamHandle,
    new_connection_handle: NewConnectionHandle,
}

impl<P> Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        stream: strategies::Stream,
        auth_con: Option<oneshot::Sender<bool>>,
        handle: &Handle,
        new_stream_handle: NewStreamHandle,
        new_connection_handle: NewConnectionHandle,
    ) -> Stream<P> {
        let state = match auth_con {
            Some(auth) => StreamState::UnAuthenticated(auth),
            None => StreamState::Authenticated,
        };

        let remote_msg = mpsc::unbounded();

        Stream {
            stream: WriteJson::new(ReadJson::new(stream)),
            state,
            remote_msg,
            con_requests: HashMap::new(),
            incoming_con_requests: HashSet::new(),
            handle: handle.clone(),
            new_stream_handle,
            new_connection_handle,
        }
    }

    /// Upgrades this `Stream` and the underlying `Connection` to be authenticated.
    /// This allows the `Connection` to accept multiple `Stream`s and the `Stream`s can execute
    /// the full protocol, e.g. opening a connection to a different device.
    pub fn upgrade_to_authenticated(&mut self) {
        match mem::replace(&mut self.state, StreamState::Authenticated){
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
                Protocol::RequestPeerConnection(connection_id, msg) => {
                    self.incoming_con_requests.insert(connection_id);
                    return Ok(Ready(Some(msg)));
                }
                Protocol::RequestPrivateAdressInformation(connection_id) => {
                    let addresses = interfaces()
                        .iter()
                        .map(|v| v.ips.clone())
                        .concat()
                        .iter()
                        .map(|v| v.ip())
                        .filter(|ip| !ip.is_loopback())
                        .map(|ip| (ip, self.stream.get_ref().get_ref().local_addr().port()).into())
                        .collect_vec();

                    self.direct_send(Protocol::PrivateAdressInformation(connection_id, addresses));
                }
                Protocol::PrivateAdressInformation(connection_id, addresses) => {
                    if let Some(handler) = self.con_requests.remove(&connection_id) {
                        handler.send_address_information(addresses);
                    }
                }
                Protocol::Connect(addresses, _, connection_id) => {}
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
        self.stream.poll()
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
        other: &mut Self,
    ) -> Result<()> {
        if !self.incoming_con_requests.remove(&connection_id) {
            bail!("unknown connection id");
        }

        let (master, slave) = ConnectToPeerCoordinator::spawn(
            &self.handle,
            connection_id,
            self.remote_msg.0.clone(),
            other.remote_msg.0.clone(),
        );

        self.con_requests.insert(connection_id, master);
        //TODO, the other `Stream` could already contain a connection request with the same id
        other.con_requests.insert(connection_id, slave);

        Ok(())
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
            match self.remote_msg.1.poll() {
                Ok(Ready(Some(msg))) => {
                    self.direct_send(msg);
                }
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
        self.stream.start_send(Protocol::Embedded(item)).map(|r| {
            r.map(|v| match v {
                Protocol::Embedded(item) => item,
                _ => unreachable!(),
            })
        })
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete()
    }
}
