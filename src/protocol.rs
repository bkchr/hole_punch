use std::net::SocketAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P> {
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
    StreamHello(Option<StreamType>),

    Connect(Vec<SocketAddr>, u32, u64),
    Embedded(P),
    RequestPeerConnection(u64, P, Vec<SocketAddr>),
    RequestRelayPeerConnection(u64),

    RequestPrivateAdressInformation,
    PrivateAdressInformation(Vec<SocketAddr>),

    ReUseConnection,
    AckReUseConnection,
    RelayModeActivated,
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

/// The type of an incoming `Stream` that describes its purpose.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum StreamType {
    /// Relay this stream to another peer.
    Relay(u64),
}
