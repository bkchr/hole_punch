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

use mdns::RecordKind;

use tokio::runtime::TaskExecutor;

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use futures::{
    future,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    Future, Poll, Stream,
};

use lru_time_cache::LruCache;

type RequestReceiver = UnboundedReceiver<(oneshot::Sender<RegistryResult>, PubKeyHash)>;

/// A registry that registers the local peer in local DNS (mDNS) and also searches for other peers
/// in local DNS.
pub struct MdnsRegistry {
    _responder: Responder,
    _services: Vec<Service>,
    request_sender: UnboundedSender<(oneshot::Sender<RegistryResult>, PubKeyHash)>,
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

        let (request_sender, request_recv) = mpsc::unbounded();

        handle.spawn(Discovery::new(
            service_name,
            Duration::from_secs(5),
            request_recv,
            local_peer_identifier,
            strategies,
        )?);

        Ok(MdnsRegistry {
            _responder: responder,
            _services: vec![service],
            request_sender,
        })
    }
}

impl RegistryProvider for MdnsRegistry {
    fn find_peer(
        &self,
        peer: &PubKeyHash,
    ) -> Box<dyn SendFuture<Item = RegistryResult, Error = ()>> {
        let (sender, recv) = oneshot::channel();
        let _ = self.request_sender.unbounded_send((sender, peer.clone()));
        Box::new(recv.map_err(|_| ()))
    }
}

/// The result found by `Discovery`.
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

/// Periodically discovers the given service via mDNS and answers peer requests.
struct Discovery {
    service_name: String,
    discovery: mdns::discover::Discovery,
    request_recv: RequestReceiver,
    local_peer_identifier: PubKeyHash,
    known_peers: LruCache<PubKeyHash, DiscoveryResult>,
    strategies: Vec<NewConnectionHandle>,
}

impl Discovery {
    /// Create a new instance of `DiscoveryStream`.
    fn new(
        service_name: String,
        interval: Duration,
        request_recv: RequestReceiver,
        local_peer_identifier: PubKeyHash,
        strategies: Vec<NewConnectionHandle>,
    ) -> Result<Self> {
        let service_name = if service_name.ends_with(".local") {
            service_name
        } else {
            format!("{}.local", service_name)
        };

        let discovery = mdns::discover::all(&service_name, interval)?;

        Ok(Self {
            discovery,
            service_name,
            request_recv,
            local_peer_identifier,
            known_peers: LruCache::with_expiry_duration_and_capacity(Duration::from_secs(60), 20),
            strategies,
        })
    }

    fn poll_results(&mut self) -> Poll<(), ()> {
        loop {
            let res = match try_ready!(self
                .discovery
                .poll()
                .map_err(|e| error!("DiscoveryStream error: {:?}", e)))
            {
                Some(res) => res,
                None => {
                    error!(target: "mdns-registry", "mDNS returned `None`");
                    return Err(());
                }
            };

            if let Some(r) = DiscoveryResult::try_from(res, &self.service_name) {
                // We don't want to add us self
                if r.peer != self.local_peer_identifier {
                    debug!(target: "mdns-registry", "mDNS found peer: {:?}", r);

                    let hash = r.peer.clone();
                    self.known_peers.insert(hash, r);
                }
            }
        }
    }
}

impl Future for Discovery {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.poll_results()?;

        loop {
            let (sender, peer) = match try_ready!(self.request_recv.poll().map_err(|_| ())) {
                Some(res) => res,
                None => return Err(()),
            };

            debug!(target: "mdns-registry", "Peer request: {}", peer);

            let addrs = match self.known_peers.get(&peer) {
                Some(peer) => {
                    let mut addrs = Vec::new();
                    peer.ports.iter().for_each(|p| {
                        addrs.extend(peer.ip_addresses.iter().map(|ip| SocketAddr::new(*ip, *p)))
                    });

                    debug!(target: "mdns-registry", "Found peer addresses: {:?}", addrs);
                    addrs
                }
                None => {
                    debug!(target: "mdns-registry", "Could not find requested peer.");
                    continue;
                }
            };

            let connections = addrs.into_iter().map(|a| {
                ConnectWithStrategies::new(
                    self.strategies.clone(),
                    a,
                    StreamHello::User(self.local_peer_identifier.clone()),
                )
            });

            tokio::spawn(
                future::select_all(connections)
                    .map(|s| {
                        let _ = sender.send(RegistryResult::Found(s.0));
                    })
                    .map_err(|_| ()),
            );
        }
    }
}
