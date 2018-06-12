use authenticator::Authenticator;
use config::Config;
use connect::ConnectWithStrategies;
use connection::{Connection, NewConnectionHandle};
use error::*;
use strategies::{self, NewConnection};
use stream::{Stream, StreamHandle};

use failure;

use std::{cmp::Eq, fmt::Debug, hash::Hash, net::SocketAddr, sync::Arc};

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream as FStream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

pub type Identifier<P, R> = Arc<<R as ResolvePeer<P>>::Identifier>;

pub struct Context<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_stream_recv: UnboundedReceiver<Stream<P, R>>,
    pass_stream_to_context: PassStreamToContext<P, R>,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle<P, R>>,
    authenticator: Option<Authenticator>,
    resolve_peer: R,
    /// The identifier of this Context.
    identifier: Identifier<P, R>,
}

impl<P, R> Context<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        identifier: R::Identifier,
        handle: Handle,
        config: Config,
        resolve_peer: R,
    ) -> Result<Context<P, R>> {
        let identifier = Arc::new(identifier);
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

        let new_connection_handles = strats
            .iter()
            .map(|s| {
                NewConnectionHandle::new(
                    s.get_new_connection_handle(),
                    pass_stream_to_context.clone(),
                    resolve_peer.clone(),
                    identifier.clone(),
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
            authenticator,
            resolve_peer,
            identifier,
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
                    let con = Connection::new(
                        con,
                        NewConnectionHandle::new(
                            strat.get_new_connection_handle(),
                            self.pass_stream_to_context.clone(),
                            self.resolve_peer.clone(),
                            self.identifier.clone(),
                            &self.handle,
                        ),
                        self.pass_stream_to_context.clone(),
                        self.resolve_peer.clone(),
                        self.identifier.clone(),
                        &self.handle,
                        false,
                    );
                    self.handle.spawn(con);
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

    pub fn create_connection_to_server(
        &mut self,
        addr: &SocketAddr,
    ) -> NewConnectionToServer<P, R> {
        NewConnectionToServer::new(ConnectWithStrategies::new(
            self.new_connection_handles.clone(),
            &self.handle,
            *addr,
        ))
    }
}

pub struct NewConnectionToServer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    connect: ConnectWithStrategies<P, R>,
}

impl<P, R> NewConnectionToServer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn new(connect: ConnectWithStrategies<P, R>) -> NewConnectionToServer<P, R> {
        NewConnectionToServer { connect }
    }
}

impl<P, R> Future for NewConnectionToServer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.connect.poll()
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
        self.new_stream_recv
            .poll()
            .map_err(|_| failure::err_msg("Could not receive new stream").into())
    }
}

#[derive(Clone)]
pub struct PassStreamToContext<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    send: UnboundedSender<Stream<P, R>>,
}

impl<P, R> PassStreamToContext<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn new(send: UnboundedSender<Stream<P, R>>) -> PassStreamToContext<P, R> {
        PassStreamToContext { send }
    }

    pub fn pass_stream(&mut self, stream: Stream<P, R>) {
        let _ = self.send.unbounded_send(stream);
    }
}

pub enum ResolvePeerResult<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    FoundLocally(StreamHandle<P, R>),
    FoundRemote(SocketAddr),
    NotFound,
}

pub trait ResolvePeer<P>: 'static + Send + Sync + Clone
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    Self::Identifier: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    type Identifier;
    fn resolve_peer(&self, peer: &Self::Identifier) -> ResolvePeerResult<P, Self>;
}
