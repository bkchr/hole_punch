use authenticator::Authenticator;
use config::Config;
use connect::ConnectWithStrategies;
use connection::{Connection, NewConnectionHandle};
use error::*;
use registry::Registry;
use strategies::{self, NewConnection};
use stream::Stream;
use PubKeyHash;

use failure;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream as FStream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

pub struct Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    new_stream_recv: UnboundedReceiver<Stream<P>>,
    pass_stream_to_context: PassStreamToContext<P>,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle<P>>,
    authenticator: Option<Authenticator>,
    registry: Registry<P>,
}

impl<P> Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(identifier: PubKeyHash, handle: Handle, config: Config) -> Result<Context<P>> {
        let registry = Registry::new(identifier);
        let (new_stream_send, new_stream_recv) = mpsc::unbounded();
        let pass_stream_to_context = PassStreamToContext::new(new_stream_send);

        let authenticator = Authenticator::new(
            config.server_ca_certificates.as_ref().cloned(),
            config.client_ca_certificates.as_ref().cloned(),
            config.authenticator_store_orig_pub_key,
        )?;

        let strats = strategies::init(handle.clone(), &config, authenticator.as_ref())?;

        let new_connection_handles = strats
            .iter()
            .map(|s| {
                NewConnectionHandle::new(
                    s.get_new_connection_handle(),
                    pass_stream_to_context.clone(),
                    registry.clone(),
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
            registry,
        })
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
                            self.handle.clone(),
                            self.authenticator.clone(),
                        ),
                        self.pass_stream_to_context.clone(),
                        self.resolve_peer.clone(),
                        self.identifier.clone(),
                        self.handle,
                        self.authenticator.clone(),
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
}

impl<P> FStream for Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;
        self.new_stream_recv
            .poll()
            .map_err(|_| failure::err_msg("Could not receive new stream").into())
    }
}

#[derive(Clone)]
pub struct PassStreamToContext<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    send: UnboundedSender<Stream<P>>,
}

impl<P> PassStreamToContext<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(send: UnboundedSender<Stream<P>>) -> PassStreamToContext<P> {
        PassStreamToContext { send }
    }

    pub fn pass_stream(&mut self, stream: Stream<P>) {
        let _ = self.send.unbounded_send(stream);
    }
}
