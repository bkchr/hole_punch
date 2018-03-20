#[derive(Deserialize, Serialize, Clone)]
pub enum Protocol {
    SendMessage(String),
    ReceiveMessage(String),
    Register(String),
    RequestPeer(String, u64),
    PeerNotFound,
}
