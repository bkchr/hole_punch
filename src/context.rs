use authenticator::Authenticator;
use config::Config;
use connect::{self, ConnectWithStrategies};
use connection_request;
use error::*;
use incoming;
use protocol::Protocol;
use strategies::{self, AddressInformation, GetConnectionId, NewConnection, NewStream};
use stream::{Stream, StreamHandle, NewStreamFuture, NewStreamHandle, get_interface_addresses};

use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem;
use std::net::SocketAddr;
use std::time::Duration;

use futures::Async::{NotReady, Ready};
use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::{mpsc, oneshot};
use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

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
        let authenticator = if config.authenticator_enable {
            Some(Authenticator::new(
                config.server_ca_certificates.as_ref().cloned(),
                config.client_ca_certificates.as_ref().cloned(),
                config.authenticator_store_orig_pub_key,
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
                        Duration::from_secs(2),
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
            get_interface_addresses(server.local_addr());
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
                        None => {
                            return Ok(Ready(Some(stream)));
                        }
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


