use PubKeyHash;

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(bound = "")]
pub enum Protocol<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    /// The first message send by each `Stream`.
    Hello(StreamPurpose<P>),

    Embedded(P),
    LocatePeer(LocatePeer<P>),
    BuildPeerToPeerConnection(BuildPeerToPeerConnection<P>),

    Error(String),
}

impl<P> From<P> for Protocol<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(msg: P) -> Protocol<P> {
        Protocol::Embedded(msg)
    }
}

/// The purpose of the Stream.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum StreamPurpose<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    /// A Stream for the user.
    User,
    /// A Stream used for the peer registry.
    Registry,
    /// Relay this stream to the given peer.
    Relay(PubKeyHash),
    /// This stream is relayed to the given peer.
    Relayed(PubKeyHash),
}

impl<P> From<StreamPurpose<P>> for Protocol<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(stype: StreamPurpose<P>) -> Protocol<P> {
        Protocol::Hello(stype)
    }
}

/// Locate a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum LocatePeer<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    Locate(PubKeyHash),
    NotFound(PubKeyHash),
    FoundLocally(PubKeyHash),
    FoundRemote(PubKeyHash, SocketAddr),
}

impl<P> From<LocatePeer<P>> for Protocol<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(info: LocatePeer<P>) -> Protocol<P> {
        Protocol::LocatePeer(info)
    }
}

/// Build a connection to a peer.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BuildPeerToPeerConnection<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    AddressInformationExchange(PubKeyHash, Vec<SocketAddr>),
    AddressInformationRequest,
    AddressInformationResponse(Vec<SocketAddr>),
    ConnectionCreate(PubKeyHash, Vec<SocketAddr>),
    PeerNotFound(PubKeyHash),
}

impl<P> From<BuildPeerToPeerConnection<P>> for Protocol<P>
where
    P: 'static + Serialize + for<'pde> Deserialize<'pde> + Clone,
{
    fn from(info: BuildPeerToPeerConnection<P>) -> Protocol<P> {
        Protocol::BuildPeerToPeerConnection(info)
    }
}
