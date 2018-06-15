use stream::StreamHandle;

use std::{
    cmp::Eq, collections::HashMap, fmt::Debug, hash::Hash, net::SocketAddr, sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use futures::{
    future, stream::{futures_unordered, FuturesUnordered}, Async::Ready, Future, Poll,
    Stream as FStream,
};

enum RegistryResult<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    FoundSelf,
    FoundLocally(StreamHandle<P, I>),
    FoundRemote(SocketAddr),
    NotFound,
}

trait RegistryProvider<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    fn find_peer(&self, peer: &I) -> Box<Future<Item = RegistryResult<P, I>, Error = ()>>;
}

struct Inner<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    /// The identifier of this peer.
    local_identifier: I,
    /// All the peers that are connected with this peer.
    connected_peers: HashMap<I, StreamHandle<P, I>>,
    /// Other remote registries
    remote_registries: Vec<Box<RegistryProvider<P, I>>>,
}

impl<P, I> RegistryProvider<P, I> for Inner<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    fn find_peer(&self, peer: &I) -> Box<Future<Item = RegistryResult<P, I>, Error = ()>> {
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

struct SearchRemoteRegistries<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    futures: FuturesUnordered<Box<Future<Item = RegistryResult<P, I>, Error = ()>>>,
}

impl<P, I> SearchRemoteRegistries<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    fn new(
        itr: impl Iterator<Item = Box<Future<Item = RegistryResult<P, I>, Error = ()>>>,
    ) -> SearchRemoteRegistries<P, I> {
        SearchRemoteRegistries {
            futures: futures_unordered(itr),
        }
    }
}

impl<P, I> Future for SearchRemoteRegistries<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    type Item = RegistryResult<P, I>;
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

struct Registry<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    inner: Arc<Mutex<Inner<P, I>>>,
}

impl<P, I> RegistryProvider<P, I> for Registry<P, I>
where
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    I: Serialize + for<'de> Deserialize<'de> + Clone + Debug + Hash + Eq,
{
    fn find_peer(&self, peer: &I) -> Box<Future<Item=RegistryResult<P, I>, Error=()>> {
        self.inner.lock().unwrap().find_peer(peer)
    }
}
