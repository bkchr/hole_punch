use stream::StreamHandle;
use PubKeyHash;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use futures::{
    future,
    stream::{futures_unordered, FuturesUnordered},
    Async::Ready,
    Future, Poll, Stream as FStream,
};

enum RegistryResult<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    FoundSelf,
    FoundLocally(StreamHandle<P>),
    FoundRemote(SocketAddr),
    NotFound,
}

trait RegistryProvider<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult<P>, Error = ()>>;
}

struct Inner<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    /// The identifier of this peer.
    local_identifier: PubKeyHash,
    /// All the peers that are connected with this peer.
    connected_peers: HashMap<PubKeyHash, StreamHandle<P>>,
    /// Other remote registries
    remote_registries: Vec<Box<RegistryProvider<P>>>,
}

impl<P> Inner<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(local_identifier: PubKeyHash) -> Inner<P> {
        Inner {
            local_identifier,
            connected_peers: HashMap::new(),
            remote_registries: Vec::new(),
        }
    }
}

impl<P> RegistryProvider<P> for Inner<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult<P>, Error = ()>> {
        if *peer == self.local_identifier {
            Box::new(future::ok(RegistryResult::FoundSelf))
        } else if let Some(handle) = self.connected_peers.get(peer) {
            Box::new(future::ok(RegistryResult::FoundLocally(handle.clone())))
        } else {
            Box::new(SearchRemoteRegistries::new(
                self.remote_registries.iter().map(|r| r.find_peer(peer)),
            ))
        }
    }
}

struct SearchRemoteRegistries<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    futures: FuturesUnordered<Box<Future<Item = RegistryResult<P>, Error = ()>>>,
}

impl<P> SearchRemoteRegistries<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        itr: impl Iterator<Item = Box<Future<Item = RegistryResult<P>, Error = ()>>>,
    ) -> SearchRemoteRegistries<P> {
        SearchRemoteRegistries {
            futures: futures_unordered(itr),
        }
    }
}

impl<P> Future for SearchRemoteRegistries<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = RegistryResult<P>;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let res = match try_ready!(self.futures.poll()) {
                Some(res) => res,
                None => return Ok(Ready(RegistryResult::NotFound)),
            };

            match res {
                RegistryResult::NotFound => {}
                r @ _ => return Ok(Ready(res)),
            }
        }
    }
}

#[derive(Clone)]
pub struct Registry<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    inner: Arc<Mutex<Inner<P>>>,
}

impl<P> Registry<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(local_identifier: PubKeyHash) -> Registry<P> {
        Registry {
            inner: Arc::new(Mutex::new(Inner::new(local_identifier))),
        }
    }
}

impl<P> RegistryProvider<P> for Registry<P>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult<P>, Error = ()>> {
        self.inner.lock().unwrap().find_peer(peer)
    }
}
