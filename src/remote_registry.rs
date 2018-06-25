use connect::ConnectWithStrategies;
use connection::NewConnectionHandle;
use error::*;
use protocol::Registry as RegistryProtocol;
use registry::{Registry, RegistryProvider, RegistryResult};
use strategies;
use stream::{NewStreamHandle, ProtocolStream};
use timeout::Timeout;
use PubKeyHash;

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
    net::SocketAddr,
    time::Duration,
};

use tokio_core::reactor::Handle;

type ResultSender = oneshot::Sender<RegistryResult>;

pub struct RemoteRegistry {
    find_peer_request: UnboundedSender<(PubKeyHash, ResultSender)>,
    handle: Handle,
}

impl RemoteRegistry {
    pub fn new(
        remote_peers: Vec<SocketAddr>,
        strategies: Vec<NewConnectionHandle>,
        local_peer_identifier: PubKeyHash,
        handle: Handle,
    ) -> RemoteRegistry {
        let (find_peer_request_send, find_peer_request_recv) = unbounded();
        let con_handler = RemoteRegistryConnectionHandler::new(
            remote_peers,
            strategies,
            local_peer_identifier,
            find_peer_request_recv,
            handle.clone(),
        );
        handle.spawn(con_handler);

        RemoteRegistry {
            handle,
            find_peer_request: find_peer_request_send,
        }
    }
}

impl RegistryProvider for RemoteRegistry {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>> {
        let (sender, receiver) = oneshot::channel();
        let _ = self
            .find_peer_request
            .unbounded_send((peer.clone(), sender));
        Box::new(TimeoutRequest::new(
            receiver,
            Duration::from_secs(20),
            &self.handle,
        ))
    }
}

struct TimeoutRequest {
    timeout: Timeout,
    result_recv: oneshot::Receiver<RegistryResult>,
}

impl TimeoutRequest {
    fn new(
        result_recv: oneshot::Receiver<RegistryResult>,
        timeout: Duration,
        handle: &Handle,
    ) -> TimeoutRequest {
        TimeoutRequest {
            result_recv,
            timeout: Timeout::new(timeout, handle),
        }
    }
}

impl Future for TimeoutRequest {
    type Item = RegistryResult;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Err(_) = self.timeout.poll() {
            println!("RemoteRegistry request timed out.");
        }

        self.result_recv.poll().map_err(|_| ())
    }
}

type FindPeerRequest = UnboundedReceiver<(PubKeyHash, ResultSender)>;

struct RemoteRegistryConnectionHandler {
    next_remote_peer: Box<dyn Iterator<Item = SocketAddr>>,
    current_peer: Option<OutgoingStream>,
    wait_for_new_peer: Option<(ConnectWithStrategies, FindPeerRequest)>,
    strategies: Vec<NewConnectionHandle>,
    handle: Handle,
    local_peer_identifier: PubKeyHash,
}

impl RemoteRegistryConnectionHandler {
    fn new(
        remote_peers: Vec<SocketAddr>,
        strategies: Vec<NewConnectionHandle>,
        local_peer_identifier: PubKeyHash,
        find_peer_request: FindPeerRequest,
        handle: Handle,
    ) -> RemoteRegistryConnectionHandler {
        let mut next_remote_peer = Box::new(remote_peers.into_iter().cycle());
        let connect = ConnectWithStrategies::new(
            strategies.clone(),
            handle.clone(),
            next_remote_peer
                .next()
                .expect("next_remote_peer should always return a `SocketAddr`."),
            local_peer_identifier.clone(),
        );

        RemoteRegistryConnectionHandler {
            next_remote_peer,
            strategies,
            local_peer_identifier,
            handle,
            current_peer: None,
            wait_for_new_peer: Some((connect, find_peer_request)),
        }
    }
}

impl Future for RemoteRegistryConnectionHandler {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            if let Some(mut current_peer) = self.current_peer.take() {
                match current_peer.poll() {
                    Ok(NotReady) => {
                        self.current_peer = Some(current_peer);
                        return Ok(NotReady);
                    }
                    Err(e) => {
                        println!(
                            "RemoteRegistryConnectionHandler current peer error: {:?}",
                            e
                        );
                    }
                    _ => {}
                };

                let find_peer_request = current_peer.into_find_peer_request();
                let connect = ConnectWithStrategies::new(
                    self.strategies.clone(),
                    self.handle.clone(),
                    self.next_remote_peer
                        .next()
                        .expect("next_remote_peer should always return a `SocketAddr`."),
                    self.local_peer_identifier.clone(),
                );
                self.wait_for_new_peer = Some((connect, find_peer_request));
            } else {
                let (mut connect, find_peer_request) = self
                    .wait_for_new_peer
                    .take()
                    .expect("wait_for_new_peer can not be None!");
                match connect.poll() {
                    Err(e) => {
                        println!("RemoteRegistryConnectionHandler connection error: {:?}", e);
                        let connect = ConnectWithStrategies::new(
                            self.strategies.clone(),
                            self.handle.clone(),
                            self.next_remote_peer
                                .next()
                                .expect("next_remote_peer should always return a `SocketAddr`."),
                            self.local_peer_identifier.clone(),
                        );
                        self.wait_for_new_peer = Some((connect, find_peer_request));
                    }
                    Ok(NotReady) => {
                        self.wait_for_new_peer = Some((connect, find_peer_request));
                        return Ok(NotReady);
                    }
                    Ok(Ready(stream)) => {
                        let new_stream_handle = stream.new_stream_handle().clone();
                        let current_peer =
                            OutgoingStream::new(stream, new_stream_handle, find_peer_request);
                        self.current_peer = Some(current_peer);
                    }
                }
            }
        }
    }
}

struct OutgoingStream {
    stream: ProtocolStream<RegistryProtocol>,
    requests: HashMap<PubKeyHash, Vec<ResultSender>>,
    new_stream_handle: NewStreamHandle,
    find_peer_request: FindPeerRequest,
}

impl OutgoingStream {
    fn new<T>(
        stream: T,
        new_stream_handle: NewStreamHandle,
        find_peer_request: FindPeerRequest,
    ) -> OutgoingStream
    where
        T: Into<ProtocolStream<RegistryProtocol>>,
    {
        OutgoingStream {
            stream: stream.into(),
            new_stream_handle,
            find_peer_request,
            requests: HashMap::new(),
        }
    }

    fn poll_find_peer_request(&mut self) -> Poll<(), Error> {
        loop {
            let (peer, sender) = match try_ready!(
                self.find_peer_request
                    .poll()
                    .map_err(|_| Error::from("poll_find_peer_request: Unknown error"))
            ) {
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
}

impl Future for OutgoingStream {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.poll_find_peer_request()?;

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
                _ => {}
            };
        }
    }
}
