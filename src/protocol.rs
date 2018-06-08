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
    ConnectionHello,

    /// The first message send by all `Stream`s of a `Connection` that follow the
    /// first `Stream`.
    /// To precisely describe the purpose of the `Stream`, it may carries a type.
    StreamHello(Option<StreamType<P, R>>),

    Embedded(P),
    LocatePeer(LocatePeer<P, R>),
    BuildPeerToPeerConnection(BuildPeerToPeerConnection<P, R>),

    ReUseConnection,
    AckReUseConnection,

    Error(String),
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

/// Locate a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum LocatePeer<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    Locate(R::Identifier),
    NotFound,
    FoundLocally(R::Identifier),
    FoundRemote(R::Identifier, SocketAddr),
}

impl<P, R> From<LocatePeer<P, R>> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(info: LocatePeer<P, R>) -> Protocol<P, R> {
        Protocol::LocatePeer(info)
    }
}

/// Build a connection to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BuildPeerToPeerConnection<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    AddressInformationExchange(R::Identifier, Vec<SocketAddr>),
    AddressInformationRequest,
    AddressInformationResponse(Vec<SocketAddr>),
    ConnectionCreate(R::Identifier, Vec<SocketAddr>),
    PeerNotFound(R::Identifier),
}

impl<P, R> From<BuildPeerToPeerConnection<P, R>> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(info: BuildPeerToPeerConnection<P, R>) -> Protocol<P, R> {
        Protocol::BuildPeerToPeerConnection(info)
    }
}
