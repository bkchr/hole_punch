use authenticator::Authenticator;
use config::Config;
use connection::{Connection, NewConnectionHandle};
use error::*;
use registry::Registry;
use strategies::{self, NewConnection};
use stream::{NewStreamHandle, Stream};
use PubKeyHash;

use failure;

use futures::{
    stream::{futures_unordered, FuturesUnordered, StreamFuture},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    Async::{NotReady, Ready},
    Poll, Stream as FStream,
};

use tokio_core::reactor::Handle;

type NewStreamChannel = (strategies::Stream, PubKeyHash, NewStreamHandle, bool);

pub struct Context {
    new_stream_recv: UnboundedReceiver<NewStreamChannel>,
    pass_stream_to_context: PassStreamToContext,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    handle: Handle,
    new_connection_handles: Vec<NewConnectionHandle>,
    authenticator: Authenticator,
    registry: Registry,
    local_peer_identifier: PubKeyHash,
}

impl Context {
    pub fn new(local_peer_identifier: PubKeyHash, handle: Handle, config: Config) -> Result<Context> {
        let registry = Registry::new(local_peer_identifier.clone());
        let (new_stream_send, new_stream_recv) = mpsc::unbounded();
        let pass_stream_to_context = PassStreamToContext::new(new_stream_send);

        let authenticator = Authenticator::new(
            config.server_ca_certificates.as_ref().cloned(),
            config.client_ca_certificates.as_ref().cloned(),
            config.authenticator_store_orig_pub_key,
        )?;

        let strats = strategies::init(handle.clone(), &config, authenticator.clone())?;

        let new_connection_handles = strats
            .iter()
            .map(|s| {
                NewConnectionHandle::new(
                    local_peer_identifier.clone(),
                    s.get_new_connection_handle(),
                    pass_stream_to_context.clone(),
                    registry.clone(),
                    handle.clone(),
                    authenticator.clone(),
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
            local_peer_identifier,
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
                        self.local_peer_identifier.clone(),
                        NewConnectionHandle::new(
                            self.local_peer_identifier.clone(),
                            strat.get_new_connection_handle(),
                            self.pass_stream_to_context.clone(),
                            self.registry.clone(),
                            self.handle.clone(),
                            self.authenticator.clone(),
                        ),
                        self.pass_stream_to_context.clone(),
                        self.registry.clone(),
                        self.handle.clone(),
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

impl FStream for Context {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;
        self.new_stream_recv
            .poll()
            .map_err(|_| failure::err_msg("Could not receive new stream").into())
            .map(|r| {
                r.map(|o| {
                    o.map(
                        |(stream, peer_identifier, new_stream_handle, is_proxy_stream)| {
                            Stream::new(stream, peer_identifier, new_stream_handle, is_proxy_stream)
                        },
                    )
                })
            })
    }
}

#[derive(Clone)]
pub struct PassStreamToContext {
    send: UnboundedSender<NewStreamChannel>,
}

impl PassStreamToContext {
    fn new(send: UnboundedSender<NewStreamChannel>) -> PassStreamToContext {
        PassStreamToContext { send }
    }

    pub fn pass_stream(
        &mut self,
        stream: strategies::Stream,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
        is_proxy_stream: bool,
    ) {
        let _ =
            self.send
                .unbounded_send((stream, peer_identifier, new_stream_handle, is_proxy_stream));
    }
}
