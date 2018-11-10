use stream::NewStreamHandle;
use PubKeyHash;

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex},
};

use futures::{
    future,
    stream::{futures_unordered, FuturesUnordered},
    Async::{NotReady, Ready},
    Future, Poll, Stream as FStream,
};

/// A token that will be returned after registering a peer. The token prevents to remove an valid
/// entry of a peer, if it is connected multiple times. Only the last registered connection, with
/// the correct token, can remove the peer from the registry.
pub type RegistrationToken = u64;

pub enum RegistryResult {
    /// Found the peer. The given handle returns new `Stream`s directly to the peer.
    Found(NewStreamHandle),
    /// Found the peer, but available at a remote peer. The given handle returns new `Stream`s to
    /// the remote peer that holds the connection to the searched peer.
    FoundRemote(NewStreamHandle),
    /// The peer could not be found.
    NotFound,
}

pub trait RegistryProvider: Send {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>>;
}

struct Inner {
    /// The identifier of this peer.
    /// All the peers that are connected with this peer.
    connected_peers: HashMap<PubKeyHash, (NewStreamHandle, RegistrationToken)>,
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
        self.connected_peers.get(peer).map(|v| v.0.clone())
    }

    fn has_peer(&self, peer: &PubKeyHash) -> bool {
        self.connected_peers.contains_key(peer)
    }

    fn register_peer(
        &mut self,
        peer: PubKeyHash,
        new_stream_handle: NewStreamHandle,
    ) -> RegistrationToken {
        match self.connected_peers.entry(peer) {
            Entry::Occupied(mut e) => {
                // Increase the token, if we already have an registered stream handle.
                let token = e.get().1 + 1;
                e.insert((new_stream_handle, token));
                token
            }
            Entry::Vacant(e) => {
                e.insert((new_stream_handle, 0));
                0
            }
        }
    }

    fn unregister_peer(&mut self, peer: PubKeyHash, token: RegistrationToken) {
        match self.connected_peers.entry(peer.clone()) {
            Entry::Occupied(e) => {
                // If the token is correct, we remove it
                if e.get().1 == token {
                    e.remove();
                }
            }
            Entry::Vacant(_) => {}
        }
    }

    pub fn add_registry_provider(&mut self, provider: impl RegistryProvider + 'static) {
        self.registries.push(Box::new(provider));
    }
}

impl RegistryProvider for Inner {
    fn find_peer(&self, peer: &PubKeyHash) -> Box<Future<Item = RegistryResult, Error = ()>> {
        if let Some(handle) = self.connected_peers.get(peer) {
            Box::new(future::ok(RegistryResult::Found(handle.0.clone())))
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
            let res = match self.futures.poll() {
                Ok(Ready(Some(res))) => res,
                Ok(Ready(None)) => return Ok(Ready(RegistryResult::NotFound)),
                Err(_) => continue,
                Ok(NotReady) => return Ok(NotReady),
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

    pub fn register_peer(
        &self,
        peer: PubKeyHash,
        new_stream_handle: NewStreamHandle,
    ) -> RegistrationToken {
        self.inner
            .lock()
            .unwrap()
            .register_peer(peer, new_stream_handle)
    }

    pub fn unregister_peer(&self, peer: PubKeyHash, token: RegistrationToken) {
        self.inner.lock().unwrap().unregister_peer(peer, token);
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
