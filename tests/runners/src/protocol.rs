#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum Protocol {
    SendMessage(String),
    ReceiveMessage(String),
}
