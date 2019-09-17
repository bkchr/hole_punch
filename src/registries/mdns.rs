use crate::{
    connect::ConnectWithStrategies,
    connection::NewConnectionHandle,
    context::SendFuture,
    error::Result,
    protocol::StreamHello,
    registry::{RegistryProvider, RegistryResult},
    PubKeyHash,
};

use libmdns::{Responder, ResponderTask, Service};

use mdns::RecordKind;

use tokio::{runtime::TaskExecutor, timer::Delay};

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use futures::{
    future,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    Async::{NotReady, Ready},
    Future, Poll, Stream,
};

use lru_time_cache::LruCache;

use state_machine_future::{transition, RentToOwn, StateMachineFuture};

type PeerRequest = (oneshot::Sender<RegistryResult>, PubKeyHash);
type RequestReceiver = UnboundedReceiver<PeerRequest>;

/// A registry that registers the local peer in local DNS (mDNS) and also searches for other peers
/// in local DNS.
pub struct MdnsRegistry {
    request_sender: UnboundedSender<(oneshot::Sender<RegistryResult>, PubKeyHash)>,
    _responder_handle: oneshot::Sender<()>,
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

        let (context, _responder_handle) = MdnsResponderContext::create(
            service_name.clone(),
            Duration::from_secs(5),
            local_peer_identifier.clone(),
            listen_port,
        );
        let responder = MdnsResponder::start(context);
        handle.spawn(responder.map_err(|_| ()));

        let (request_sender, request_recv) = mpsc::unbounded();

        handle.spawn(Discovery::new(
            service_name,
            Duration::from_secs(5),
            request_recv,
            local_peer_identifier,
            strategies,
        )?);

        Ok(MdnsRegistry {
            request_sender,
            _responder_handle,
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
    initial_delay: Delay,
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
        let initial_delay = Delay::new(Instant::now() + Duration::from_secs(5));

        Ok(Self {
            discovery,
            service_name,
            request_recv,
            local_peer_identifier,
            known_peers: LruCache::with_expiry_duration_and_capacity(Duration::from_secs(60), 20),
            strategies,
            initial_delay,
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

        match self.initial_delay.poll() {
            Ok(NotReady) => return Ok(NotReady),
            Err(e) => {
                error!(target: "mdns-registry", "Faulty timer: {:?}", e);
            }
            Ok(Ready(_)) => {}
        }

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

enum Never {}

struct MdnsResponderContext {
    /// Time to wait before retrying the registration of the responder.
    wait_before_retry: Duration,
    /// The identifier of this peer.
    local_peer_identifier: String,
    service_name: String,
    /// Tells us when we should shutdown.
    shutdown_signal: oneshot::Receiver<()>,
    listen_port: u16,
}

impl MdnsResponderContext {
    fn create(
        service_name: String,
        wait_before_retry: Duration,
        local_peer_identifier: PubKeyHash,
        listen_port: u16,
    ) -> (Self, oneshot::Sender<()>) {
        // The hash is to long and we need to split it up.
        let local_peer_identifier_string = local_peer_identifier.to_string();
        let local_peer_identifier = format!(
            "{}.{}",
            &local_peer_identifier_string[..local_peer_identifier_string.len() / 2],
            &local_peer_identifier_string[local_peer_identifier_string.len() / 2..],
        );

        let (sender, shutdown_signal) = oneshot::channel();
        (
            Self {
                service_name,
                shutdown_signal,
                wait_before_retry,
                local_peer_identifier,
                listen_port,
            },
            sender,
        )
    }

    fn is_shutdown(&mut self) -> bool {
        self.shutdown_signal
            .poll()
            .map(|v| v.is_ready())
            .unwrap_or(true)
    }
}

/// MDNs responder state machine. This makes sure we have the responder registered/reregistered.
///
/// The responder can only be registered when we have a valid network device with some connection to
/// something. So, if any error occurs while registering/polling we just retry after some timeout.
#[derive(StateMachineFuture)]
#[state_machine_future(context = "MdnsResponderContext")]
enum MdnsResponder {
    /// Registers the responder.
    ///
    /// On success it jumps to `PollResponder`.
    ///
    /// On error it jumps to `WaitForRetry`.
    ///
    /// Jumps to `ReadyState` when we should shutdown.
    #[state_machine_future(start, transitions(PollResponder, WaitForRetry, ReadyState))]
    RegisterResponder,
    /// Polls the responder.
    ///
    /// On return value that is not `NotReady` it jumps to `WaitForRetry`.
    ///
    /// Jumps to `ReadyState` when we should shutdown.
    #[state_machine_future(transitions(WaitForRetry, ReadyState))]
    PollResponder {
        _responder: Responder,
        responder_task: ResponderTask,
        _service: Service,
    },
    #[state_machine_future(transitions(RegisterResponder, ReadyState))]
    WaitForRetry(Delay),
    #[state_machine_future(ready)]
    ReadyState(()),
    #[state_machine_future(error)]
    ErrorState(Never),
}

impl PollMdnsResponder for MdnsResponder {
    fn poll_register_responder<'a, 'c>(
        _: &'a mut RentToOwn<'a, RegisterResponder>,
        context: &'c mut RentToOwn<'c, MdnsResponderContext>,
    ) -> Poll<AfterRegisterResponder, Never> {
        if context.is_shutdown() {
            transition!(ReadyState(()))
        }

        if let Ok((_responder, responder_task)) = Responder::create() {
            let _service = _responder.register(
                context.service_name.clone(),
                context.local_peer_identifier.clone(),
                context.listen_port,
                &[],
            );

            transition!(PollResponder {
                _responder,
                responder_task,
                _service
            })
        } else {
            transition!(WaitForRetry(Delay::new(
                Instant::now() + context.wait_before_retry
            )))
        }
    }

    fn poll_poll_responder<'a, 'c>(
        state: &'a mut RentToOwn<'a, PollResponder>,
        context: &'c mut RentToOwn<'c, MdnsResponderContext>,
    ) -> Poll<AfterPollResponder, Never> {
        if context.is_shutdown() {
            transition!(ReadyState(()))
        }

        if state
            .responder_task
            .poll()
            .map(|v| v.is_ready())
            .unwrap_or(true)
        {
            transition!(WaitForRetry(Delay::new(
                Instant::now() + context.wait_before_retry
            )))
        } else {
            Ok(NotReady)
        }
    }

    fn poll_wait_for_retry<'a, 'c>(
        state: &'a mut RentToOwn<'a, WaitForRetry>,
        context: &'c mut RentToOwn<'c, MdnsResponderContext>,
    ) -> Poll<AfterWaitForRetry, Never> {
        if context.is_shutdown() {
            transition!(ReadyState(()))
        }

        if state.0.poll().map(|v| v.is_ready()).unwrap_or(true) {
            transition!(RegisterResponder)
        } else {
            Ok(NotReady)
        }
    }
}
