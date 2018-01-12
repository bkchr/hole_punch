use std::net::SocketAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P> {
    Register,
    Connect(Vec<SocketAddr>, u32, u64),
    KeepAlive,
    Embedded(P),
    Acknowledge,
    RequestPrivateAdressInformation(u64),
    PrivateAdressInformation(u64, Vec<SocketAddr>),
    Hello,
    HelloAck,
    ReUseConnection,
    AckReUseConnection,
    RelayConnection(u64),
    RelayModeActivated,
}
