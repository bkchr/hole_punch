use PubKeyHash;

use std::net::SocketAddr;

/// The first message send by each `Stream` that defines its purpose
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum StreamHello {
    /// A Stream for the user that comes from the given peer.
    User(PubKeyHash),
    /// A Stream for the user that should be proxied to the given remote peer.
    UserProxy(PubKeyHash),
    /// A Stream used by the peer registry.
    Registry,
    /// A Stream used for building a connection to another peer.
    BuildConnectionToPeer(PubKeyHash),
    /// A Stream used for building a connection to another peer that should be proxied.
    ProxyBuildConnectionToPeer(PubKeyHash),
}

/// The procotol used by `Stream`s that serve as registry providers.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Registry {
    /// Request the given peer.
    Find(PubKeyHash),
    /// The given peer was not found.
    NotFound(PubKeyHash),
    /// Found the requested peer.
    Found(PubKeyHash),
    /// A Ping message send to the other side of the registry connection.
    Ping,
    /// An answer to a `Ping` message.
    Pong,
}

/// Build a connection to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BuildConnectionToPeer {
    /// The address information of the peer, as seen by the peer in the middle.
    InternetAddressInformation(SocketAddr),
    /// Exchange the address information with the other side.
    ExchangeAddressInformation(Vec<SocketAddr>),
    /// Use the current connection as proxy connection.
    ProxyConnection,
}
