use std::net::SocketAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P> {
    /// Represents an incoming connection request
    RequestConnection,
    /// Represents an incoming poke connection that can be ignored.
    PokeConnection,
    PeerToPeerConnection(u64),
    RelayConnection(u64),
    ConnectionEstablished,
    ConnectionSelected,

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
