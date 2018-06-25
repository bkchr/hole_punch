use stream::NewStreamHandle;
use PubKeyHash;

use std::{
    collections::HashMap, sync::{Arc, Mutex},
};

use futures::{
    future, stream::{futures_unordered, FuturesUnordered}, Async::Ready, Future, Poll,
    Stream as FStream,
};

pub enum RegistryResult {
    /// Found the peer. The given handle returns new `Stream`s directly to the peer.
    Found(NewStreamHandle),
    /// Found the peer, but available at a remote peer. The given handle returns new `Stream`s to
    /// the remote peer that holds the connection to the searched peer.
    FoundRemote(NewStreamHandle),
    /// The peer could not be found.
    NotFound,
}

pub trait RegistryProvider {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>>;
}

struct Inner {
    /// The identifier of this peer.
    /// All the peers that are connected with this peer.
    connected_peers: HashMap<PubKeyHash, NewStreamHandle>,
    /// Other registries
    registries: Vec<Box<RegistryProvider>>,
}

impl Inner {
    fn new() -> Inner {
        Inner {
            connected_peers: HashMap::new(),
            registries: Vec::new(),
        }
    }

    fn peer(&self, peer: &PubKeyHash) -> Option<NewStreamHandle> {
        self.connected_peers.get(peer).cloned()
    }

    fn has_peer(&self, peer: &PubKeyHash) -> bool {
        self.connected_peers.contains_key(peer)
    }

    fn register_peer(&mut self, peer: PubKeyHash, new_stream_handle: NewStreamHandle) {
        self.connected_peers.insert(peer, new_stream_handle);
    }

    fn unregister_peer(&mut self, peer: &PubKeyHash) {
        self.connected_peers.remove(peer);
    }

    pub fn add_registry_provider(&mut self, provider: impl RegistryProvider + 'static) {
        self.registries.push(Box::new(provider));
    }
}

impl RegistryProvider for Inner {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>> {
        if let Some(handle) = self.connected_peers.get(peer) {
            Box::new(future::ok(RegistryResult::Found(handle.clone())))
        } else {
            Box::new(SearchRemoteRegistries::new(
                self.registries.iter().map(|r| r.find_peer(peer)),
            ))
        }
    }
}

struct SearchRemoteRegistries {
    futures: FuturesUnordered<Box<Future<Item = RegistryResult, Error = ()>>>,
}

impl SearchRemoteRegistries {
    fn new(
        itr: impl Iterator<Item = Box<Future<Item = RegistryResult, Error = ()>>>,
    ) -> SearchRemoteRegistries {
        SearchRemoteRegistries {
            futures: futures_unordered(itr),
        }
    }
}

impl Future for SearchRemoteRegistries {
    type Item = RegistryResult;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let res = match try_ready!(self.futures.poll()) {
                Some(res) => res,
                None => return Ok(Ready(RegistryResult::NotFound)),
            };

            match res {
                RegistryResult::NotFound => {}
                r @ _ => return Ok(Ready(r)),
            }
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            inner: Arc::new(Mutex::new(Inner::new())),
        }
    }

    pub fn peer(&self, peer: &PubKeyHash) -> Option<NewStreamHandle> {
        self.inner.lock().unwrap().peer(peer)
    }

    pub fn has_peer(&self, peer: &PubKeyHash) -> bool {
        self.inner.lock().unwrap().has_peer(peer)
    }

    pub fn register_peer(&self, peer: PubKeyHash, new_stream_handle: NewStreamHandle) {
        self.inner
            .lock()
            .unwrap()
            .register_peer(peer, new_stream_handle);
    }

    pub fn unregister_peer(&self, peer: &PubKeyHash) {
        self.inner.lock().unwrap().unregister_peer(peer);
    }

    pub fn add_registry_provider(&self, provider: impl RegistryProvider + 'static) {
        self.inner.lock().unwrap().add_registry_provider(provider);
    }
}

impl RegistryProvider for Registry {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>> {
        self.inner.lock().unwrap().find_peer(peer)
    }
}
