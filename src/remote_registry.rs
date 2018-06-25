use connect::ConnectWithStrategies;
use error::*;
use protocol::Registry as RegistryProtocol;
use registry::{Registry, RegistryProvider, RegistryResult};
use strategies;
use stream::{NewStreamHandle, ProtocolStream};
use timeout::Timeout;
use PubKeyHash;

use futures::{
    future::Either, sync::{
        mpsc::{UnboundedReceiver, UnboundedSender}, oneshot,
    },
    Async::{NotReady, Ready}, Future, Poll, Sink, Stream as FStream,
};

use std::{
    collections::{hash_map::Entry, HashMap}, net::SocketAddr, time::Duration,
};

use tokio_core::reactor::Handle;

type ResultSender = oneshot::Sender<RegistryResult>;

struct RemoteRegistry {
    find_peer_request: UnboundedSender<(PubKeyHash, ResultSender)>,
}

impl RegistryProvider for RemoteRegistry {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>> {
        let (sender, receiver) = oneshot::channel();
        let _ = self
            .find_peer_request
            .unbounded_send((peer.clone(), sender));
        Box::new(receiver.map_err(|_| ()))
    }
}

struct TimeoutRequest {
    timeout: Timeout,
    result_sender: ResultSender,
    requested_peer: PubKeyHash,
}

impl TimeoutRequest {
    fn new(
        requested_peer: PubKeyHash,
        result_sender: ResultSender,
        timeout: Duration,
        handle: &Handle,
    ) -> TimeoutRequest {
        TimeoutRequest {
            result_sender,
            requested_peer,
            timeout: Timeout::new(timeout, handle),
        }
    }

    fn into_inner(self) -> (PubKeyHash, ResultSender) {
        (self.requested_peer, self.result_sender)
    }
}

impl Future for TimeoutRequest {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.timeout.poll() {
            Err(_) => return Ok(Ready(())),
            _ => return Ok(NotReady),
        }
    }
}

struct RemoteRegistryConnectionHandler {
    next_remote_peer: Box<dyn Iterator<Item = SocketAddr>>,
    current_peer: Either<ConnectWithStrategies, OutgoingStream>,
}

struct OutgoingStream {
    stream: ProtocolStream<RegistryProtocol>,
    find_peer_request: UnboundedReceiver<(PubKeyHash, ResultSender)>,
    requests: HashMap<PubKeyHash, Vec<ResultSender>>,
    new_stream_handle: NewStreamHandle,
}

impl OutgoingStream {
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
