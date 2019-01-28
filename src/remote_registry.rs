use crate::connect::ConnectWithStrategies;
use crate::connection::NewConnectionHandle;
use crate::context::SendFuture;
use crate::error::*;
use crate::protocol::{Registry as RegistryProtocol, StreamHello};
use crate::registry::{Registry, RegistryProvider, RegistryResult};
use crate::strategies;
use crate::stream::{NewStreamHandle, ProtocolStream};
use crate::timeout::Timeout;
use crate::PubKeyHash;

use futures::{
    sync::{
        mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    Async::{NotReady, Ready},
    Future, Poll, Sink, Stream as FStream,
};

use std::{
    collections::{hash_map::Entry, HashMap},
    net::{SocketAddr, ToSocketAddrs},
    time::{Duration, Instant},
};

use tokio::{
    runtime::TaskExecutor,
    timer::{Delay, Interval},
};

use state_machine_future::{RentToOwn, StateMachineFuture, transition};

type ResultSender = oneshot::Sender<RegistryResult>;

/// A common trait for resolving `Url`s and `SocketAddr`s to `SocketAddr`s.
pub trait Resolve: Send {
    /// Resolve the addresses, if possible.
    fn resolve(&self) -> Result<Vec<SocketAddr>>;
}

impl<T: ToSocketAddrs + Send> Resolve for T {
    fn resolve(&self) -> Result<Vec<SocketAddr>> {
        self.to_socket_addrs()
            .map(|v| v.collect::<Vec<_>>())
            .map_err(|e| e.into())
    }
}

pub struct RemoteRegistry {
    find_peer_request: UnboundedSender<(PubKeyHash, ResultSender)>,
}

impl RemoteRegistry {
    pub fn new(
        resolvers: Vec<Box<dyn Resolve>>,
        ping_interval: Duration,
        address_resolve_timeout: Duration,
        strategies: Vec<NewConnectionHandle>,
        local_peer_identifier: PubKeyHash,
        handle: TaskExecutor,
    ) -> RemoteRegistry {
        let (find_peer_request_send, find_peer_request_recv) = unbounded();
        let con_handler = ConnectionHandler::new(
            resolvers,
            strategies,
            local_peer_identifier,
            find_peer_request_recv,
            ping_interval,
            address_resolve_timeout,
        );
        handle.spawn(con_handler.map_err(|_| ()).map(|_| ()));

        RemoteRegistry {
            find_peer_request: find_peer_request_send,
        }
    }
}

impl RegistryProvider for RemoteRegistry {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<dyn SendFuture<Item = RegistryResult, Error = ()>> {
        let (sender, receiver) = oneshot::channel();
        self.find_peer_request
            .unbounded_send((peer.clone(), sender))
            .expect("ConnectionHandler should never end!");
        Box::new(TimeoutRequest::new(receiver, Duration::from_secs(10)))
    }
}

struct TimeoutRequest {
    timeout: Timeout,
    result_recv: oneshot::Receiver<RegistryResult>,
}

impl TimeoutRequest {
    fn new(result_recv: oneshot::Receiver<RegistryResult>, timeout: Duration) -> TimeoutRequest {
        TimeoutRequest {
            result_recv,
            timeout: Timeout::new(timeout),
        }
    }
}

impl Future for TimeoutRequest {
    type Item = RegistryResult;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Err(_) = self.timeout.poll() {
            error!("RemoteRegistry request timed out.");
            Err(())?;
        }

        self.result_recv.poll().map_err(|_| ())
    }
}

type FindPeerRequest = UnboundedReceiver<(PubKeyHash, ResultSender)>;

struct GetNextAddr {
    resolvers: Vec<Box<dyn Resolve>>,
    resolved_addrs: Vec<SocketAddr>,
    last_resolver: usize,
    /// Timeout between calling the resolvers.
    timeout: Timeout,
}

impl GetNextAddr {
    fn new(resolvers: Vec<Box<dyn Resolve>>, timeout: Duration) -> Self {
        let resolved_addrs = resolvers[0].resolve().unwrap_or_else(|_| Vec::new());

        GetNextAddr {
            resolvers,
            resolved_addrs,
            last_resolver: 0,
            timeout: Timeout::new(timeout),
        }
    }
}

impl Future for GetNextAddr {
    type Item = SocketAddr;
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Check in maximum one extra resolver per call.
        for _ in 0..2 {
            if !self.resolved_addrs.is_empty() {
                return Ok(Ready(self.resolved_addrs.pop().unwrap()));
            } else if self.timeout.poll().is_err() {
                self.timeout.reset();

                let next_resolver = if self.last_resolver + 1 == self.resolvers.len() {
                    0
                } else {
                    self.last_resolver + 1
                };

                self.resolved_addrs = self.resolvers[next_resolver]
                    .resolve()
                    .unwrap_or_else(|_| Vec::new());
                self.last_resolver = next_resolver;
            }
        }

        Ok(NotReady)
    }
}

enum Never {}

struct ConnectionHandlerContext {
    next_peer_addr: GetNextAddr,
    strategies: Vec<NewConnectionHandle>,
    local_peer_identifier: PubKeyHash,
    ping_interval: Duration,
}

/// Handles the connection to the remote peer.
#[derive(StateMachineFuture)]
#[state_machine_future(context = "ConnectionHandlerContext")]
enum ConnectionHandler {
    /// Request the next peer address.
    #[state_machine_future(start, transitions(ConnectToPeer))]
    RequestNextPeerAddr { find_peer_request: FindPeerRequest },
    /// Connect to a peer.
    #[state_machine_future(transitions(HandleConnection, RequestNextPeerAddr))]
    ConnectToPeer {
        connect: ConnectWithStrategies,
        find_peer_request: FindPeerRequest,
    },
    /// We have a connection to a peer and just poll this connection.
    #[state_machine_future(transitions(RequestNextPeerAddr, ReadyState))]
    HandleConnection { connection: OutgoingStream },
    #[state_machine_future(ready)]
    ReadyState(Never),
    #[state_machine_future(error)]
    ErrorState(Never),
}

impl ConnectionHandler {
    fn new(
        resolvers: Vec<Box<dyn Resolve>>,
        strategies: Vec<NewConnectionHandle>,
        local_peer_identifier: PubKeyHash,
        find_peer_request: FindPeerRequest,
        ping_interval: Duration,
        address_resolve_timeout: Duration,
    ) -> ConnectionHandlerFuture {
        let next_peer_addr = GetNextAddr::new(resolvers, address_resolve_timeout);
        let context = ConnectionHandlerContext {
            next_peer_addr,
            strategies,
            local_peer_identifier,
            ping_interval,
        };

        Self::start(find_peer_request, context)
    }
}

impl PollConnectionHandler for ConnectionHandler {
    fn poll_request_next_peer_addr<'a, 'c>(
        state: &'a mut RentToOwn<'a, RequestNextPeerAddr>,
        context: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterRequestNextPeerAddr, Never> {
        let addr = match context.next_peer_addr.poll() {
            Ok(Ready(addr)) => addr,
            _ => return Ok(NotReady),
        };

        info!("ConnectionHandler connects to: {}", addr);

        let find_peer_request = state.take().find_peer_request;
        let connect = ConnectWithStrategies::new(
            context.strategies.clone(),
            addr,
            context.local_peer_identifier.clone(),
            StreamHello::Registry,
        );
        transition!(ConnectToPeer {
            connect,
            find_peer_request
        })
    }

    fn poll_connect_to_peer<'a, 'c>(
        state: &'a mut RentToOwn<'a, ConnectToPeer>,
        context: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterConnectToPeer, Never> {
        let stream = match state.connect.poll() {
            Ok(Ready(stream)) => stream,
            Err(e) => {
                error!("ConnectionHandler connect error: {:?}", e);

                let find_peer_request = state.take().find_peer_request;

                transition!(RequestNextPeerAddr { find_peer_request })
            }
            Ok(NotReady) => return Ok(NotReady),
        };

        info!("ConnectionHandler connected to peer.");

        let find_peer_request = state.take().find_peer_request;
        let new_stream_handle = stream.new_stream_handle().clone();
        let connection = OutgoingStream::new(
            stream,
            new_stream_handle,
            find_peer_request,
            context.ping_interval.clone(),
        );
        transition!(HandleConnection { connection })
    }

    fn poll_handle_connection<'a, 'c>(
        state: &'a mut RentToOwn<'a, HandleConnection>,
        _: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterHandleConnection, Never> {
        match state.connection.poll() {
            Ok(Ready(())) => {}
            Err(e) => {
                error!("ConnectionHandler connection error: {:?}", e);
            }
            Ok(NotReady) => return Ok(NotReady),
        };

        let find_peer_request = state.take().connection.into_find_peer_request();
        transition!(RequestNextPeerAddr { find_peer_request })
    }
}

struct OutgoingStream {
    stream: ProtocolStream<RegistryProtocol>,
    requests: HashMap<PubKeyHash, Vec<ResultSender>>,
    new_stream_handle: NewStreamHandle,
    find_peer_request: FindPeerRequest,
    ping_interval: Interval,
    pong_timeout: Delay,
}

impl OutgoingStream {
    fn new<T>(
        stream: T,
        new_stream_handle: NewStreamHandle,
        find_peer_request: FindPeerRequest,
        ping_interval: Duration,
    ) -> OutgoingStream
    where
        T: Into<ProtocolStream<RegistryProtocol>>,
    {
        let pong_interval = ping_interval * 3;
        OutgoingStream {
            stream: stream.into(),
            new_stream_handle,
            find_peer_request,
            requests: HashMap::new(),
            ping_interval: Interval::new(Instant::now(), ping_interval),
            pong_timeout: Delay::new(Instant::now() + pong_interval),
        }
    }

    fn poll_find_peer_request(&mut self) -> Poll<(), Error> {
        loop {
            let (peer, sender) = match try_ready!(self
                .find_peer_request
                .poll()
                .map_err(|_| Error::from("poll_find_peer_request: Unknown error")))
            {
                Some(req) => req,
                None => {
                    return Ok(Ready(()));
                }
            };

            if sender.is_canceled() {
                continue;
            }

            match self.requests.entry(peer.clone()) {
                Entry::Occupied(mut e) => {
                    e.get_mut().push(sender);
                }
                Entry::Vacant(e) => {
                    e.insert(vec![sender]);
                    self.stream.start_send(RegistryProtocol::Find(peer))?;
                    self.stream.poll_complete()?;
                }
            }
        }
    }

    fn into_find_peer_request(self) -> FindPeerRequest {
        self.find_peer_request
    }

    fn poll_ping_and_pong(&mut self) -> Result<()> {
        if let Ready(Some(_)) = self.ping_interval.poll()? {
            self.stream.start_send(RegistryProtocol::Ping)?;
            self.stream.poll_complete()?;
        }

        self.pong_timeout.poll().map(|_| ()).map_err(Into::into)
    }
}

impl Future for OutgoingStream {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.poll_find_peer_request()?;
        self.poll_ping_and_pong()?;

        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(())),
            };

            match msg {
                RegistryProtocol::Found(peer) => {
                    if let Some(requests) = self.requests.remove(&peer) {
                        requests.into_iter().for_each(|req| {
                            let _ = req
                                .send(RegistryResult::FoundRemote(self.new_stream_handle.clone()));
                        });
                    }
                }
                RegistryProtocol::NotFound(peer) => {
                    if let Some(requests) = self.requests.remove(&peer) {
                        requests.into_iter().for_each(|req| {
                            let _ = req.send(RegistryResult::NotFound);
                        });
                    }
                }
                RegistryProtocol::Pong => {
                    self.pong_timeout
                        .reset(Instant::now() + Duration::from_secs(3));
                }
                _ => {}
            };
        }
    }
}

pub struct IncomingStream {
    stream: ProtocolStream<RegistryProtocol>,
    registry: Registry,
}

impl IncomingStream {
    pub fn new<T>(stream: T, registry: Registry) -> IncomingStream
    where
        T: Into<strategies::Stream>,
    {
        IncomingStream {
            stream: stream.into().into(),
            registry,
        }
    }
}

impl Future for IncomingStream {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(())),
            };

            match msg {
                RegistryProtocol::Find(peer) => {
                    let answer = if self.registry.has_peer(&peer) {
                        RegistryProtocol::Found(peer)
                    } else {
                        RegistryProtocol::NotFound(peer)
                    };
                    self.stream.start_send(answer)?;
                    self.stream.poll_complete()?;
                }
                RegistryProtocol::Ping => {
                    self.stream.start_send(RegistryProtocol::Pong)?;
                    self.stream.poll_complete()?;
                }
                _ => {}
            };
        }
    }
}
