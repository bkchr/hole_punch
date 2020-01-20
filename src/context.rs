use crate::authenticator::Authenticator;
use crate::build_connection_to_peer::BuildConnectionToPeer;
use crate::config::Config;
use crate::connection::{Connection, NewConnectionHandle};
use crate::error::*;
use crate::registries::{MdnsRegistry, RemoteRegistry};
use crate::registry::{Registry, RegistryProvider, RegistryResult};
use crate::strategies::{self, LocalAddressInformation, NewConnection};
use crate::stream::{Stream};
use crate::PubKeyHash;

use failure;

use futures::{
    stream::{futures_unordered, FuturesUnordered, StreamFuture},
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    Async::{NotReady, Ready},
    Future, IntoFuture, Poll, Stream as FStream,
};

use std::{net::SocketAddr, time::Duration};

use tokio::{self, runtime::TaskExecutor};

/// A `Future` that implements `Send`.
pub trait SendFuture: Future + Send {}
impl<T: Future + Send> SendFuture for T {}

pub struct Context {
    new_stream_recv: UnboundedReceiver<Stream>,
    new_connection_handles: Vec<NewConnectionHandle>,
    registry: Registry,
    local_peer_identifier: PubKeyHash,
    quic_local_addr: SocketAddr,
    context_inner_handle: oneshot::Receiver<()>,
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
            let remote_registry = RemoteRegistry::new(
                config.remote_peers,
                config.remote_registry_ping_interval,
                config.remote_registry_address_resolve_timeout,
                new_connection_handles.clone(),
                handle.clone(),
            );
            registry.add_registry_provider(remote_registry);
        }

        if let Some(service_name) = config.enable_mdns {
            let mdns_registry = MdnsRegistry::new(
                local_peer_identifier.clone(),
                new_connection_handles.clone(),
                &service_name,
                quic_local_addr.port(),
                handle.clone(),
            )?;
            registry.add_registry_provider(mdns_registry);
        }

        let (sender, context_inner_handle) = oneshot::channel();

        handle.spawn(ContextInner::new(
            pass_stream_to_context,
            strats,
            registry.clone(),
            local_peer_identifier.clone(),
            authenticator,
            sender,
        ));

        Ok(Context {
            new_stream_recv,
            new_connection_handles,
            registry,
            local_peer_identifier,
            quic_local_addr,
            context_inner_handle,
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

    pub fn local_peer_identifier(&self) -> &PubKeyHash {
        &self.local_peer_identifier
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.registry.context_being_dropped();
    }
}

fn create_connection_to_peer(
    peer: PubKeyHash,
    new_con_handles: &[NewConnectionHandle],
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
                    RegistryResult::Found(stream) => Ok(Box::new(Ok(stream).into_future())),
                    RegistryResult::FoundRemote(new_stream_handle) => {
                        Ok(Box::new(BuildConnectionToPeer::new(
                            local_peer_identifier,
                            peer,
                            new_connection_handle,
                            new_stream_handle,
                            switch_to_proxy_timeout,
                        )))
                    }
                    RegistryResult::FoundWithHandle(mut handle) => {
                        Ok(Box::new(handle.new_stream()))
                    }
                    RegistryResult::NotFound => Err(Error::PeerNotFound(peer)),
                }
            },
        )
        .map_err(|e| {
            trace!(target: "Context", "Create connection to peer error: {:?}", e);
            e
        })
        .flatten()
}

impl FStream for Context {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self
            .context_inner_handle
            .poll()
            .map(|v| v.is_ready())
            .unwrap_or(true)
        {
            return Err(Error::from("HolePunch::ContextInner dropped!"));
        }

        self.new_stream_recv
            .poll()
            .map_err(|_| failure::err_msg("Could not receive new stream").into())
    }
}

#[derive(Clone)]
pub struct PassStreamToContext {
    send: UnboundedSender<Stream>,
}

impl PassStreamToContext {
    fn new(send: UnboundedSender<Stream>) -> PassStreamToContext {
        PassStreamToContext { send }
    }

    pub fn pass_stream(&mut self, stream: Stream) {
        let _ = self.send.unbounded_send(stream);
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

struct ContextInner {
    pass_stream_to_context: PassStreamToContext,
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    registry: Registry,
    local_peer_identifier: PubKeyHash,
    authenticator: Authenticator,
    drop_handle: Option<oneshot::Sender<()>>,
}

impl ContextInner {
    fn new(
        pass_stream_to_context: PassStreamToContext,
        strategies: Vec<strategies::Strategy>,
        registry: Registry,
        local_peer_identifier: PubKeyHash,
        authenticator: Authenticator,
        drop_handle: oneshot::Sender<()>,
    ) -> Self {
        Self {
            pass_stream_to_context,
            strategies: futures_unordered(strategies.into_iter().map(|s| s.into_future())),
            registry,
            local_peer_identifier,
            authenticator,
            drop_handle: Some(drop_handle),
        }
    }
}

impl Future for ContextInner {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self
            .drop_handle
            .as_mut()
            .and_then(|h| h.poll_cancel().ok())
            .map(|r| r.is_ready())
            .unwrap_or(true)
        {
            trace!("Context dropped, will drop ContextInner");
            return Ok(Ready(()));
        }

        loop {
            match self.strategies.poll() {
                Ok(NotReady) => return Ok(NotReady),
                Err(e) => {
                    error!("Strategy returned error: {:?}", e.0);
                    return Ok(Ready(()));
                }
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
                    error!("Strategy returned None!");
                    return Ok(Ready(()));
                }
                Ok(Ready(None)) => {
                    error!("Strategies empty!");
                    return Ok(Ready(()));
                }
            }
        }
    }
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        if let Some(h) = self.drop_handle.take() {
            let _ = h.send(());
        }
    }
}
