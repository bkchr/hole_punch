use authenticator::Authenticator;
use build_connection_to_peer::BuildConnectionToPeer;
use config::Config;
use connection::{Connection, NewConnectionHandle};
use error::*;
use registry::{Registry, RegistryProvider, RegistryResult};
use remote_registry;
use strategies::{self, LocalAddressInformation, NewConnection};
use stream::{NewStreamHandle, Stream};
use PubKeyHash;

use failure;

use futures::{
    stream::{futures_unordered, FuturesUnordered, StreamFuture},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender}, Async::{NotReady, Ready}, Future, Poll,
    Stream as FStream,
};

use std::{net::SocketAddr, time::Duration};

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
    quic_local_addr: SocketAddr,
}

impl Context {
    pub fn new(
        local_peer_identifier: PubKeyHash,
        handle: Handle,
        config: Config,
    ) -> Result<Context> {
        let registry = Registry::new();
        let (new_stream_send, new_stream_recv) = mpsc::unbounded();
        let pass_stream_to_context = PassStreamToContext::new(new_stream_send);

        let authenticator = Authenticator::new(
            config.outgoing_ca_certificates.clone(),
            config.incoming_ca_certificates.clone(),
            true,
        )?;

        let strats = strategies::init(handle.clone(), &config, authenticator.clone())?;

        // TODO!!!
        let quic_local_addr = strats.get(0).unwrap().local_addr();

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
            .collect::<Vec<_>>();

        if !config.remote_peers.is_empty() {
            let remote_registry = remote_registry::RemoteRegistry::new(
                config.remote_peers.clone(),
                new_connection_handles.clone(),
                local_peer_identifier.clone(),
                handle.clone(),
            );
            registry.add_registry_provider(remote_registry);
        }

        Ok(Context {
            new_stream_recv,
            pass_stream_to_context,
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
            handle,
            new_connection_handles,
            authenticator,
            registry,
            local_peer_identifier,
            quic_local_addr,
        })
    }

    pub fn create_connection_to_peer(
        &self,
        peer: PubKeyHash,
    ) -> impl Future<Item = Stream, Error = Error> {
        create_connection_to_peer(
            peer,
            self.handle.clone(),
            &self.new_connection_handles,
            &self.registry,
            self.local_peer_identifier.clone(),
        )
    }

    pub fn create_connection_to_peer_handle(&self) -> CreateConnectionToPeerHandle {
        CreateConnectionToPeerHandle::new(
            self.registry.clone(),
            self.new_connection_handles.clone(),
            self.handle.clone(),
            self.local_peer_identifier.clone(),
        )
    }

    pub fn quic_local_addr(&self) -> SocketAddr {
        self.quic_local_addr
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

fn create_connection_to_peer(
    peer: PubKeyHash,
    handle: Handle,
    new_con_handles: &Vec<NewConnectionHandle>,
    registry: &Registry,
    local_peer_identifier: PubKeyHash,
) -> impl Future<Item = Stream, Error = Error> {
    let handle = handle.clone();
    // TODO: Don't do that.
    let new_connection_handle = new_con_handles.get(0).unwrap().clone();

    registry
        .find_peer(&peer)
        .map_err(|_| Error::from("Unknown error while finding a peer"))
        .and_then(
            move |find| -> Result<Box<Future<Item = Stream, Error = Error>>> {
                match find {
                    RegistryResult::Found(mut new_stream_handle) => {
                        return Ok(Box::new(new_stream_handle.new_stream()))
                    }
                    RegistryResult::FoundRemote(new_stream_handle) => {
                        return Ok(Box::new(BuildConnectionToPeer::new(
                            local_peer_identifier,
                            peer,
                            new_connection_handle,
                            new_stream_handle,
                            Duration::from_secs(20),
                            handle,
                        )))
                    }
                    RegistryResult::NotFound => bail!("Could not find peer: {:?}", peer),
                }
            },
        )
        .flatten()
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

#[derive(Clone)]
pub struct CreateConnectionToPeerHandle {
    handle: Handle,
    registry: Registry,
    new_con_handles: Vec<NewConnectionHandle>,
    local_peer_identifier: PubKeyHash,
}

impl CreateConnectionToPeerHandle {
    fn new(
        registry: Registry,
        new_con_handles: Vec<NewConnectionHandle>,
        handle: Handle,
        local_peer_identifier: PubKeyHash,
    ) -> CreateConnectionToPeerHandle {
        CreateConnectionToPeerHandle {
            registry,
            new_con_handles,
            handle,
            local_peer_identifier,
        }
    }

    pub fn create_connection_to_peer(
        &self,
        peer: PubKeyHash,
    ) -> impl Future<Item = Stream, Error = Error> {
        create_connection_to_peer(
            peer,
            self.handle.clone(),
            &self.new_con_handles,
            &self.registry,
            self.local_peer_identifier.clone(),
        )
    }
}
