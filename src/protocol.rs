use context::ResolvePeer;

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    /// The first message send by a `Connection` in the first `Stream`.
    /// To precisely describe the purpose of the `Connection`, it may carries a type.
    ConnectionHello(Option<ConnectionType>),
    /// Acknowledge the `ConnectionHello`.
    ConnectionHelloAck,
    /// The messages follows a `ConnectionHello`, if the type is `PeerToPeer`.
    /// As the master peer connects to all available IP addresses of the slave peer,
    /// it may happens that multiple `Connection`s are built.
    /// This message indicates that the `Connection` is the one that will be used
    /// by the master peer.
    ConnectionSelected,

    /// The first message send by all `Stream`s of a `Connection` that follow the
    /// first `Stream`.
    /// To precisely describe the purpose of the `Stream`, it may carries a type.
    StreamHello(Option<StreamType<P, R>>),

    Embedded(P),
    PeerInfo(PeerInfo<P, R>),

    RequestPrivateAdressInformation,
    PrivateAdressInformation(Vec<SocketAddr>),

    ReUseConnection,
    AckReUseConnection,
}

impl<P, R> From<P> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(msg: P) -> Protocol<P, R> {
        Protocol::Embedded(msg)
    }
}

/// The type of an incoming `Connection` that describe its purpose.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ConnectionType {
    /// A peer to peer connection. This type is used by the peer that initiated
    /// this connection.
    PeerToPeer(u64),
    /// A peer to peer poke connection. This type is used by the peer that did not initiate
    /// this connection.
    PeerToPeerPoke,
}

impl<P, R> From<ConnectionType> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(ctype: ConnectionType) -> Protocol<P, R> {
        Protocol::ConnectionHello(Some(ctype))
    }
}

/// The type of an incoming `Stream` that describes its purpose.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum StreamType<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    /// Relay this stream to another peer.
    Relay(R::Identifier),
}

impl<P, R> From<StreamType<P, R>> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(stype: StreamType<P, R>) -> Protocol<P, R> {
        Protocol::StreamHello(Some(stype))
    }
}

/// Build a connection to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum PeerInfo<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    Request(R::Identifier, Vec<SocketAddr>),
    NotFound,
    FoundLocally(R::Identifier, Vec<SocketAddr>),
    FoundRemote(R::Identifier, SocketAddr),
}

impl<P, R> From<PeerInfo<P, R>> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(info: PeerInfo<P, R>) -> Protocol<P, R> {
        Protocol::PeerInfo(info)
    }
}
