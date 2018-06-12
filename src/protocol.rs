use context::ResolvePeer;

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(bound = "")]
pub enum Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    /// The first message send by in each `Stream`.
    Hello(Option<StreamType<P, R>>),

    Embedded(P),
    LocatePeer(LocatePeer<P, R>),
    BuildPeerToPeerConnection(BuildPeerToPeerConnection<P, R>),

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
    /// It holds the name of the origin peer and the remote peer.
    Relay(R::Identifier, R::Identifier),
    /// This stream is relayed by another peer.
    /// It holds the name of the remote peer.
    Relayed(R::Identifier),
}

impl<P, R> From<StreamType<P, R>> for Protocol<P, R>
where
    R: ResolvePeer<P>,
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(stype: StreamType<P, R>) -> Protocol<P, R> {
        Protocol::Hello(Some(stype))
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
    NotFound(R::Identifier),
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
