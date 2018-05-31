use authenticator::Authenticator;
use config::Config;
use connect::{self, ConnectWithStrategies};
use connection::{Connection, ConnectionId, NewConnectionHandle};
use error::*;
use incoming;
use protocol::Protocol;
use strategies::{self, NewConnection};
use stream::{get_interface_addresses, Stream, StreamHandle};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::{mpsc, oneshot};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream as FStream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

use rand::{self, Rng};

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

        let addresses = get_interface_addresses(server.local_addr());
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
