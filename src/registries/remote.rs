use crate::{
    connect::ConnectWithStrategies,
    connection::NewConnectionHandle,
    context::SendFuture,
    error::*,
    protocol::{Registry as RegistryProtocol, StreamHello},
    registry::{Registry, RegistryProvider, RegistryResult},
    strategies,
    stream::{NewStreamHandle, ProtocolStrategiesStream},
    timeout::Timeout,
    PubKeyHash,
};

use trust_dns_resolver::{
    config::{ResolverConfig, ResolverOpts},
    AsyncResolver,
};

use futures::{
    stream::{self, Fuse},
    sync::{
        mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    Async::{NotReady, Ready},
    Future, Poll, Sink, Stream as FStream,
};

use std::{
    collections::{hash_map::Entry, HashMap},
    convert::AsRef,
    net::SocketAddr,
    time::{Duration, Instant},
};

use tokio::{
    runtime::TaskExecutor,
    timer::{Delay, Interval},
};

use state_machine_future::{transition, RentToOwn, StateMachineFuture};

type ResultSender = oneshot::Sender<RegistryResult>;

/// A common trait for resolving `Url`s and `SocketAddr`s to `SocketAddr`s.
pub trait Resolve: Send + 'static {
    /// Resolve the addresses, if possible.
    fn resolve(
        &self,
        handle: TaskExecutor,
    ) -> Box<dyn FStream<Item = SocketAddr, Error = ()> + Send>;
}

impl Resolve for SocketAddr {
    fn resolve(&self, _: TaskExecutor) -> Box<dyn FStream<Item = SocketAddr, Error = ()> + Send> {
        Box::new(stream::once(Ok(*self)))
    }
}

impl<T: AsRef<str> + Send + 'static> Resolve for (T, u16) {
    fn resolve(
        &self,
        handle: TaskExecutor,
    ) -> Box<dyn FStream<Item = SocketAddr, Error = ()> + Send> {
        let (resolver, background) =
            AsyncResolver::new(ResolverConfig::default(), ResolverOpts::default());

        handle.spawn(background);

        let port = self.1;
        Box::new(
            resolver
                .lookup_ip(format!("{}.", self.0.as_ref()).as_str())
                .and_then(|addresses| Ok(stream::iter_ok(addresses.into_iter())))
                .flatten_stream()
                .map(move |ip| (ip, port).into())
                .map_err(|_| ()),
        )
    }
}

/// A registry that connects to a remote peer to query it for searched peers.
pub struct RemoteRegistry {
    find_peer_request: UnboundedSender<(PubKeyHash, ResultSender)>,
    /// Will make sure that the `ConnectionHandlerContext` is dropped the `RemoteRegistry` is
    /// dropped.
    _context_handle: oneshot::Receiver<()>,
}

impl RemoteRegistry {
    pub fn new(
        resolvers: Vec<Box<dyn Resolve>>,
        ping_interval: Duration,
        address_resolve_timeout: Duration,
        strategies: Vec<NewConnectionHandle>,
        handle: TaskExecutor,
    ) -> RemoteRegistry {
        let (find_peer_request_send, find_peer_request_recv) = unbounded();
        let (context_handle, _context_handle) = oneshot::channel();

        let con_handler = ConnectionHandler::new(
            resolvers,
            strategies,
            find_peer_request_recv,
            ping_interval,
            address_resolve_timeout,
            context_handle,
            handle.clone(),
        );
        handle.spawn(con_handler.map_err(|_| ()).map(|_| ()));

        RemoteRegistry {
            find_peer_request: find_peer_request_send,
            _context_handle,
        }
    }
}

impl RegistryProvider for RemoteRegistry {
    fn find_peer(
        &self,
        peer: &PubKeyHash,
    ) -> Box<dyn SendFuture<Item = RegistryResult, Error = ()>> {
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
        if self.timeout.poll().is_err() {
            error!("RemoteRegistry request timed out.");
            Err(())?;
        }

        self.result_recv.poll().map_err(|_| ())
    }
}

type FindPeerRequest = UnboundedReceiver<(PubKeyHash, ResultSender)>;

struct GetNextAddr {
    /// All resolvers for ip addresses.
    resolvers: Vec<Box<dyn Resolve>>,
    /// A `Stream` that resolves to `SocketAddr`'s.
    resolve_addrs: Fuse<Box<dyn FStream<Item = SocketAddr, Error = ()> + Send>>,
    /// Duration to wait between calling `resolve` after `resolve_addrs` ended.
    wait_between_resolve_duration: Duration,
    wait_between_resolve: Delay,
    handle: TaskExecutor,
}

/// Converts the given resolvers to a `Stream` that resolves addresses using these resolvers.
fn resolvers_to_stream<'a>(
    mut resolvers: impl Iterator<Item = &'a Box<dyn Resolve>>,
    handle: TaskExecutor,
) -> Fuse<Box<dyn FStream<Item = SocketAddr, Error = ()> + Send>> {
    let first = resolvers
        .next()
        .expect("`resolvers` need to contain at least one element")
        .resolve(handle.clone());

    resolvers
        .fold(first, |o, c| Box::new(o.select(c.resolve(handle.clone()))))
        .fuse()
}

impl GetNextAddr {
    fn new(
        resolvers: Vec<Box<dyn Resolve>>,
        wait_between_resolve_duration: Duration,
        handle: TaskExecutor,
    ) -> Self {
        let resolve_addrs = resolvers_to_stream(resolvers.iter(), handle.clone());
        let wait_between_resolve = Delay::new(Instant::now() + wait_between_resolve_duration);
        GetNextAddr {
            resolvers,
            resolve_addrs,
            wait_between_resolve,
            wait_between_resolve_duration,
            handle,
        }
    }
}

impl FStream for GetNextAddr {
    type Item = SocketAddr;
    type Error = Never;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match self.resolve_addrs.poll() {
                Ok(Ready(Some(addr))) => return Ok(Ready(Some(addr))),
                Ok(Ready(None)) => match self.wait_between_resolve.poll() {
                    Err(_) | Ok(Ready(_)) => {
                        self.resolve_addrs =
                            resolvers_to_stream(self.resolvers.iter(), self.handle.clone());
                        self.wait_between_resolve
                            .reset(Instant::now() + self.wait_between_resolve_duration);
                    }
                    Ok(NotReady) => return Ok(NotReady),
                },
                Err(_) => {}
                Ok(NotReady) => return Ok(NotReady),
            }
        }
    }
}

enum Never {}

struct ConnectionHandlerContext {
    next_peer_addr: GetNextAddr,
    strategies: Vec<NewConnectionHandle>,
    ping_interval: Duration,
    remote_registry_handle: oneshot::Sender<()>,
}

impl ConnectionHandlerContext {
    fn remote_registry_dropped(&mut self) -> Result<()> {
        if self
            .remote_registry_handle
            .poll_cancel()
            .map(|h| h.is_ready())
            .unwrap_or(true)
        {
            bail!("Remote registry dropped")
        } else {
            Ok(())
        }
    }
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
    ErrorState(Error),
}

impl ConnectionHandler {
    fn new(
        resolvers: Vec<Box<dyn Resolve>>,
        strategies: Vec<NewConnectionHandle>,
        find_peer_request: FindPeerRequest,
        ping_interval: Duration,
        address_resolve_timeout: Duration,
        remote_registry_handle: oneshot::Sender<()>,
        handle: TaskExecutor,
    ) -> ConnectionHandlerFuture {
        let next_peer_addr = GetNextAddr::new(resolvers, address_resolve_timeout, handle);
        let context = ConnectionHandlerContext {
            next_peer_addr,
            strategies,
            ping_interval,
            remote_registry_handle,
        };

        Self::start(find_peer_request, context)
    }
}

impl PollConnectionHandler for ConnectionHandler {
    fn poll_request_next_peer_addr<'a, 'c>(
        state: &'a mut RentToOwn<'a, RequestNextPeerAddr>,
        context: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterRequestNextPeerAddr, Error> {
        let addr = match context.next_peer_addr.poll() {
            Ok(Ready(addr)) => addr.expect("`GetNextAddr` always returns an address."),
            _ => return Ok(NotReady),
        };

        context.remote_registry_dropped()?;

        info!("ConnectionHandler connects to: {}", addr);

        let find_peer_request = state.take().find_peer_request;
        let connect =
            ConnectWithStrategies::new(context.strategies.clone(), addr, StreamHello::Registry);
        transition!(ConnectToPeer {
            connect,
            find_peer_request
        })
    }

    fn poll_connect_to_peer<'a, 'c>(
        state: &'a mut RentToOwn<'a, ConnectToPeer>,
        context: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterConnectToPeer, Error> {
        context.remote_registry_dropped()?;

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
        let peer_identifier = stream.peer_identifier().clone();
        let connection = OutgoingStream::new(
            stream,
            new_stream_handle,
            find_peer_request,
            context.ping_interval,
            peer_identifier,
        );
        transition!(HandleConnection { connection })
    }

    fn poll_handle_connection<'a, 'c>(
        state: &'a mut RentToOwn<'a, HandleConnection>,
        context: &'c mut RentToOwn<'c, ConnectionHandlerContext>,
    ) -> Poll<AfterHandleConnection, Error> {
        context.remote_registry_dropped()?;
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
    stream: ProtocolStrategiesStream<RegistryProtocol>,
    requests: HashMap<PubKeyHash, Vec<ResultSender>>,
    new_stream_handle: NewStreamHandle,
    find_peer_request: FindPeerRequest,
    ping_interval: Interval,
    pong_timeout: Delay,
    pong_timeout_duration: Duration,
    peer_identifier: PubKeyHash,
}

impl OutgoingStream {
    fn new<T>(
        stream: T,
        new_stream_handle: NewStreamHandle,
        find_peer_request: FindPeerRequest,
        ping_interval: Duration,
        peer_identifier: PubKeyHash,
    ) -> OutgoingStream
    where
        T: Into<ProtocolStrategiesStream<RegistryProtocol>>,
    {
        let pong_timeout_duration = ping_interval * 3;
        OutgoingStream {
            stream: stream.into(),
            new_stream_handle,
            find_peer_request,
            requests: HashMap::new(),
            ping_interval: Interval::new(Instant::now(), ping_interval),
            pong_timeout: Delay::new(Instant::now() + pong_timeout_duration),
            pong_timeout_duration,
            peer_identifier,
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

            if peer == self.peer_identifier {
                info!("Found peer({}) as remote registry provider locally.", peer);
                let _ = sender.send(RegistryResult::FoundWithHandle(
                    self.new_stream_handle.clone(),
                ));
            } else {
                match self.requests.entry(peer.clone()) {
                    Entry::Occupied(mut e) => {
                        info!("Already send remote request for peer: {}", peer);
                        e.get_mut().push(sender);
                    }
                    Entry::Vacant(e) => {
                        info!("Sending remote request for peer: {}", peer);
                        e.insert(vec![sender]);
                        self.stream.start_send(RegistryProtocol::Find(peer))?;
                        self.stream.poll_complete()?;
                    }
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

        if let Ready(()) = self.pong_timeout.poll()? {
            bail!("OutgoingStream timeout");
        }

        Ok(())
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
                    info!("Found peer({}) remote.", peer);
                    if let Some(requests) = self.requests.remove(&peer) {
                        requests.into_iter().for_each(|req| {
                            let _ = req
                                .send(RegistryResult::FoundRemote(self.new_stream_handle.clone()));
                        });
                    }
                }
                RegistryProtocol::NotFound(peer) => {
                    info!("Could not find peer({}) remote.", peer);
                    if let Some(requests) = self.requests.remove(&peer) {
                        requests.into_iter().for_each(|req| {
                            let _ = req.send(RegistryResult::NotFound);
                        });
                    }
                }
                RegistryProtocol::Pong => {
                    self.pong_timeout
                        .reset(Instant::now() + self.pong_timeout_duration);
                    self.pong_timeout.poll()?;
                    if self.pong_timeout.is_elapsed() {
                        panic!("OugoingStream pong timeout instantly elapsed!")
                    }
                }
                _ => {}
            };
        }
    }
}

pub struct IncomingStream {
    stream: ProtocolStrategiesStream<RegistryProtocol>,
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
                    info!("RemoteRequest: Searching for peer: {}", peer);
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};
    use tokio::prelude::StreamExt;

    struct Resolver {
        res: Arc<Mutex<Option<SocketAddr>>>,
    }

    impl Resolve for Resolver {
        fn resolve(
            &self,
            _: TaskExecutor,
        ) -> Box<dyn FStream<Item = SocketAddr, Error = ()> + Send> {
            match *self.res.lock().unwrap() {
                Some(addr) => Box::new(stream::once(Ok(addr))),
                None => Box::new(stream::once(Err(()))),
            }
        }
    }

    #[test]
    fn get_next_addr() {
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        let res = Arc::new(Mutex::new(None));
        let socket_addr = SocketAddr::from(([127, 0, 0, 1], 5555));

        assert!(runtime
            .block_on(
                GetNextAddr::new(
                    vec![Box::new(Resolver { res: res.clone() })],
                    Duration::from_nanos(1000),
                    runtime.executor(),
                )
                .timeout(Duration::from_secs(1))
                .into_future()
            )
            .map_err(|e| e.0)
            .map(|e| e.0)
            .unwrap_err()
            .is_elapsed());

        *res.lock().unwrap() = Some(socket_addr);

        assert_eq!(
            runtime
                .block_on(
                    GetNextAddr::new(
                        vec![Box::new(Resolver { res: res.clone() })],
                        Duration::from_nanos(1000),
                        runtime.executor(),
                    )
                    .timeout(Duration::from_secs(1))
                    .into_future()
                )
                .map_err(|_| panic!())
                .map(|e| e.0)
                .unwrap(),
            Some(socket_addr),
        );
    }

    #[test]
    fn resolves_example_dot_com() {
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        let get_next_addr = GetNextAddr::new(
            vec![Box::new(("example.com", 80))],
            Duration::from_nanos(1000),
            runtime.executor(),
        );

        let address = runtime
            .block_on(get_next_addr.into_future())
            .map(|r| r.0)
            .map_err(|_| panic!("`GetNextAddr` never returns an error"))
            .unwrap()
            .unwrap();

        if address.is_ipv4() {
            assert_eq!(address, ([93, 184, 216, 34], 80).into());
        } else {
            assert_eq!(
                address,
                (
                    [0x2606, 0x2800, 0x220, 0x1, 0x248, 0x1893, 0x25c8, 0x1946],
                    80
                )
                    .into(),
            );
        }
    }
}
