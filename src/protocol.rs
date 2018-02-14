use std::net::SocketAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P> {
    /// Represents an incoming connection request
    RequestConnection,
    /// Represents an incoming poke connection that can be ignored. 
    PokeConnection,
    PeerToPeerConnection(u64),
    ConnectionEstablished,

    Connect(Vec<SocketAddr>, u32, u64),
    Embedded(P),
    RequestPeerConnection(u64, P),

    RequestPrivateAdressInformation(u64),
    PrivateAdressInformation(u64, Vec<SocketAddr>),

    ReUseConnection,
    AckReUseConnection,
    RelayConnection(u64),
    RelayModeActivated,
}
