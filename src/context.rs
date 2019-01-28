use crate::authenticator::Authenticator;
use crate::build_connection_to_peer::BuildConnectionToPeer;
use crate::config::Config;
use crate::connection::{Connection, NewConnectionHandle};
use crate::error::*;
use crate::registry::{Registry, RegistryProvider, RegistryResult};
use crate::remote_registry;
use crate::strategies::{self, LocalAddressInformation, NewConnection};
use crate::stream::{NewStreamHandle, Stream};
use crate::PubKeyHash;

use failure;

use futures::{
    stream::{futures_unordered, FuturesUnordered, StreamFuture},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    Async::{NotReady, Ready},
    Future, Poll, Stream as FStream,
};

use std::{net::SocketAddr, time::Duration};

use tokio::{self, runtime::TaskExecutor};

type NewStreamChannel = (strategies::Stream, PubKeyHash, NewStreamHandle, bool);

/// A `Future` that implements `Send`.
pub trait SendFuture: Future + Send {}
impl<T: Future + Send> SendFuture for T {}

pub struct Context {
    new_stream_recv: UnboundedReceiver<NewStreamChannel>,
    pass_stream_to_context: PassStreamToContext,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    new_connection_handles: Vec<NewConnectionHandle>,
    authenticator: Authenticator,
    registry: Registry,
    local_peer_identifier: PubKeyHash,
    quic_local_addr: SocketAddr,
}

impl Context {
    pub fn new(
        local_peer_identifier: PubKeyHash,
        handle: TaskExecutor,
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
                    authenticator.clone(),
                )
            })
            .collect::<Vec<_>>();

        if !config.remote_peers.is_empty() {
            let remote_registry = remote_registry::RemoteRegistry::new(
                config.remote_peers,
                config.remote_registry_ping_interval,
                config.remote_registry_address_resolve_timeout,
                new_connection_handles.clone(),
                local_peer_identifier.clone(),
                handle,
            );
            registry.add_registry_provider(remote_registry);
        }

        Ok(Context {
            new_stream_recv,
            pass_stream_to_context,
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
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
    ) -> impl SendFuture<Item = Stream, Error = Error> {
        self.create_connection_to_peer_with_custom_timeout(peer, Duration::from_secs(4))
    }

    pub fn create_connection_to_peer_with_custom_timeout(
        &self,
        peer: PubKeyHash,
        switch_to_proxy_timeout: Duration,
    ) -> impl SendFuture<Item = Stream, Error = Error> {
        create_connection_to_peer(
            peer,
            &self.new_connection_handles,
            &self.registry,
            self.local_peer_identifier.clone(),
            switch_to_proxy_timeout,
        )
    }

    pub fn create_connection_to_peer_handle(&self) -> CreateConnectionToPeerHandle {
        CreateConnectionToPeerHandle::new(
            self.registry.clone(),
            self.new_connection_handles.clone(),
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
                            self.authenticator.clone(),
                        ),
                        self.pass_stream_to_context.clone(),
                        self.registry.clone(),
                        self.authenticator.clone(),
                    );

                    match con {
                        Ok(con) => {
                            tokio::spawn(con);
                        }
                        Err(e) => error!("{:?}", e),
                    }
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

    pub fn local_peer_identifier(&self) -> &PubKeyHash {
        &self.local_peer_identifier
    }
}

fn create_connection_to_peer(
    peer: PubKeyHash,
    new_con_handles: &Vec<NewConnectionHandle>,
    registry: &Registry,
    local_peer_identifier: PubKeyHash,
    switch_to_proxy_timeout: Duration,
) -> impl SendFuture<Item = Stream, Error = Error> {
    // TODO: Don't do that.
    let new_connection_handle = new_con_handles.get(0).unwrap().clone();

    registry
        .find_peer(&peer)
        .map_err(|_| Error::from("Unknown error while finding a peer"))
        .and_then(
            move |find| -> Result<Box<dyn SendFuture<Item = Stream, Error = Error>>> {
                match find {
                    RegistryResult::Found(mut new_stream_handle) => {
                        Ok(Box::new(new_stream_handle.new_stream()))
                    }
                    RegistryResult::FoundRemote(new_stream_handle) => {
                        Ok(Box::new(BuildConnectionToPeer::new(
                            local_peer_identifier,
                            peer,
                            new_connection_handle,
                            new_stream_handle,
                            switch_to_proxy_timeout,
                        )))
                    }
                    RegistryResult::NotFound => Err(Error::PeerNotFound(peer)),
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
    registry: Registry,
    new_con_handles: Vec<NewConnectionHandle>,
    local_peer_identifier: PubKeyHash,
}

impl CreateConnectionToPeerHandle {
    fn new(
        registry: Registry,
        new_con_handles: Vec<NewConnectionHandle>,
        local_peer_identifier: PubKeyHash,
    ) -> CreateConnectionToPeerHandle {
        CreateConnectionToPeerHandle {
            registry,
            new_con_handles,
            local_peer_identifier,
        }
    }

    pub fn create_connection_to_peer(
        &self,
        peer: PubKeyHash,
    ) -> impl SendFuture<Item = Stream, Error = Error> {
        self.create_connection_to_peer_with_custom_timeout(peer, Duration::from_secs(4))
    }

    pub fn create_connection_to_peer_with_custom_timeout(
        &self,
        peer: PubKeyHash,
        switch_to_proxy_timeout: Duration,
    ) -> impl SendFuture<Item = Stream, Error = Error> {
        create_connection_to_peer(
            peer,
            &self.new_con_handles,
            &self.registry,
            self.local_peer_identifier.clone(),
            switch_to_proxy_timeout,
        )
    }
}
