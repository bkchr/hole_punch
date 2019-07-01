use crate::{
    connect::ConnectWithStrategies,
    connection::NewConnectionHandle,
    context::SendFuture,
    error::Result,
    protocol::StreamHello,
    registry::{RegistryProvider, RegistryResult},
    PubKeyHash,
};

use libmdns::{Responder, Service};

use mdns::{discover::Discovery, RecordKind};

use tokio::runtime::TaskExecutor;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use parking_lot::RwLock;

use futures::{future, sync::oneshot, Async::Ready, Future, IntoFuture, Poll, Stream};

use lru_time_cache::LruCache;

/// A registry that registers the local peer in local DNS (mDNS) and also searches for other peers
/// in local DNS.
pub struct MdnsRegistry {
    _responder: Responder,
    _services: Vec<Service>,
    _discovery_handle: oneshot::Sender<()>,
    known_peers: Arc<RwLock<LruCache<PubKeyHash, DiscoveryResult>>>,
    strategies: Vec<NewConnectionHandle>,
    local_peer_identifier: PubKeyHash,
}

impl MdnsRegistry {
    /// Creates a new `MdnsRegistry` instance.
    pub fn new(
        local_peer_identifier: PubKeyHash,
        strategies: Vec<NewConnectionHandle>,
        service_name: &str,
        listen_port: u16,
        handle: TaskExecutor,
    ) -> Result<Self> {
        let service_name = if service_name.starts_with('_') {
            service_name.to_owned()
        } else {
            format!("_{}", service_name)
        };

        let local_peer_identifier_string = local_peer_identifier.to_string();
        let responder = Responder::new()?;
        let service = responder.register(
            service_name.clone(),
            // The hash is to long and we need to split it up.
            format!(
                "{}.{}",
                &local_peer_identifier_string[..local_peer_identifier_string.len() / 2],
                &local_peer_identifier_string[local_peer_identifier_string.len() / 2..],
            ),
            listen_port,
            &[],
        );

        let known_peers = Arc::new(RwLock::new(LruCache::with_expiry_duration_and_capacity(
            Duration::from_secs(60),
            20,
        )));

        let known_peers_clone = known_peers.clone();
        let local_peer_identifier_clone = local_peer_identifier.clone();

        let (_discovery_handle, recv) = oneshot::channel();
        handle.spawn(
            DiscoveryStream::new(service_name, Duration::from_secs(5))?
                .for_each(move |r| {
                    // We don't want to add us self
                    if r.peer != local_peer_identifier_clone {
                        let hash = r.peer.clone();
                        known_peers_clone.write().insert(hash, r);
                    }
                    Ok(())
                })
                .select(recv.map_err(|_| ()))
                .map_err(|_| ())
                .map(|_| ()),
        );

        Ok(MdnsRegistry {
            _responder: responder,
            _services: vec![service],
            _discovery_handle,
            known_peers,
            local_peer_identifier,
            strategies,
        })
    }
}

impl RegistryProvider for MdnsRegistry {
    fn find_peer(
        &self,
        peer: &PubKeyHash,
    ) -> Box<dyn SendFuture<Item = RegistryResult, Error = ()>> {
        let addrs = if let Some(peer) = self.known_peers.write().get(peer) {
            let mut addrs = Vec::new();
            peer.ports.iter().for_each(|p| {
                addrs.extend(peer.ip_addresses.iter().map(|ip| SocketAddr::new(*ip, *p)))
            });
            addrs
        } else {
            return Box::new(Err(()).into_future());
        };

        let connections = addrs.into_iter().map(|a| {
            ConnectWithStrategies::new(
                self.strategies.clone(),
                a,
                StreamHello::User(self.local_peer_identifier.clone()),
            )
        });
        Box::new(
            future::select_all(connections)
                .map(|s| RegistryResult::Found(s.0))
                .map_err(|_| ()),
        )
    }
}

/// The result found by `DiscoveryStream`.
#[derive(Debug)]
struct DiscoveryResult {
    peer: PubKeyHash,
    ports: Vec<u16>,
    ip_addresses: Vec<IpAddr>,
}

impl DiscoveryResult {
    fn try_from(resp: mdns::Response, service_name: &str) -> Option<Self> {
        let mut peer = None;
        let mut ports = Vec::new();
        let mut ip_addresses: Vec<IpAddr> = Vec::new();

        for record in resp.records() {
            match &record.kind {
                RecordKind::A(addr) => ip_addresses.push(IpAddr::from(*addr)),
                RecordKind::AAAA(addr) => ip_addresses.push(IpAddr::from(*addr)),
                RecordKind::SRV { port, .. } => ports.push(*port),
                RecordKind::PTR(hash) => {
                    // Not the service we are interested in.
                    if !record.name.starts_with(service_name) {
                        return None;
                    }

                    if let Some(pos) = hash.find(&format!(".{}", record.name)) {
                        peer = PubKeyHash::from_hashed_hex(&hash[..pos].replace('.', "")).ok();
                    }
                }
                _ => {}
            }
        }

        peer.map(|peer| Self {
            peer,
            ports,
            ip_addresses,
        })
    }
}

/// Periodically discovers the given service via mDNS.
struct DiscoveryStream {
    service_name: String,
    discovery: Discovery,
}

impl DiscoveryStream {
    /// Create a new instance of `DiscoveryStream`.
    fn new(service_name: String, interval: Duration) -> Result<Self> {
        let service_name = if service_name.ends_with(".local") {
            service_name
        } else {
            format!("{}.local", service_name)
        };

        let discovery = mdns::discover::all(&service_name, interval)?;

        Ok(Self {
            discovery,
            service_name,
        })
    }
}

impl Stream for DiscoveryStream {
    type Item = DiscoveryResult;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let res = match try_ready!(self
                .discovery
                .poll()
                .map_err(|e| error!("DiscoveryStream error: {:?}", e)))
            {
                Some(res) => res,
                None => return Ok(Ready(None)),
            };

            if let Some(r) = DiscoveryResult::try_from(res, &self.service_name) {
                debug!("mDNS found peer: {:?}", r);
                return Ok(Ready(Some(r)));
            }
        }
    }
}
