use authenticator::Authenticator;
use config::Config;
use connect::{self, ConnectWithStrategies};
use connection::{Connection, ConnectionId, NewConnectionHandle};
use error::*;
use protocol::Protocol;
use strategies::{self, NewConnection};
use stream::{get_interface_addresses, Stream, StreamHandle};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use std::fmt::Debug;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender}, oneshot,
};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream as FStream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

pub struct Context<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_stream_recv: UnboundedReceiver<Stream<P, R>>,
    pass_stream_to_context: PassStreamToContext<P>,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle<P, R>>,
    device_to_device_callback: (
        mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P, R>)>,
        mpsc::UnboundedReceiver<(Vec<SocketAddr>, ConnectionId, StreamHandle<P, R>)>,
    ),
    outgoing_device_con: FuturesUnordered<connect::DeviceToDeviceConnectionFuture<P>>,
    new_connection: (
        mpsc::UnboundedSender<Connection<P, R>>,
        mpsc::UnboundedReceiver<Connection<P, R>>,
    ),
    authenticator: Option<Authenticator>,
    resolve_peer: R,
}

impl<P, R> Context<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(handle: Handle, config: Config, resolve_peer: R) -> Result<Context<P, R>> {
        let (new_stream_send, new_stream_recv) = mpsc::unbounded();
        let pass_stream_to_context = PassStreamToContext::new(new_stream_send);

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
                    pass_stream_to_context.clone(),
                    &handle,
                )
            })
            .collect();

        Ok(Context {
            new_stream_recv,
            pass_stream_to_context,
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
            handle,
            new_connection_handles,
            device_to_device_callback,
            outgoing_device_con: FuturesUnordered::new(),
            new_connection: mpsc::unbounded(),
            incoming_stream: FuturesUnordered::new(),
            authenticator,
            resolve_peer,
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
                    // self.incoming.push(incoming::Handler::new(
                    //     Connection::new(con, self.pass_stream_to_context.clone(), &self.handle),
                    //     Duration::from_secs(2),
                    //     &self.handle,
                    // ));
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
                self.handle.spawn(con);
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
                    self.handle.spawn(con);
                }
                _ => {
                    return;
                }
            }
        }
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
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (con, stream) = try_ready!(self.connect.poll());

        let _ = self.new_con_send.unbounded_send(con);

        Ok(Ready(stream))
    }
}

impl<P, R> FStream for Context<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;
        self.poll_device_to_device_callback();
        self.poll_new_connection();

        self.poll_outgoing_device_connections()
    }
}

#[derive(Clone)]
pub struct PassStreamToContext<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    send: UnboundedSender<Stream<P, R>>,
}

impl<P> PassStreamToContext<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(send: UnboundedSender<Stream<P, R>>) -> PassStreamToContext<P> {
        PassStreamToContext { send }
    }

    pub fn pass_stream(&mut self, stream: Stream<P, R>) {
        let _ = self.send.unbounded_send(stream);
    }
}

pub enum ResolvePeerResult<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    Found(StreamHandle<P, R>),
    NotFound,
    NotFoundLocally(SocketAddr),
}

pub trait ResolvePeer<P>: Send + Sync + Clone
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    Self::Identifier: Serialize + for<'de> Deserialize<'de> + Clone + Debug,
{
    type Identifier;
    fn resolve_peer(&self, peer: &Self::Identifier) -> ResolvePeerResult<P, Self>;
}
