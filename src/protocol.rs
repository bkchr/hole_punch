use std::net::IpAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AddressInformation {
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol<P> {
    Register,
    Connect {
        public: AddressInformation,
        private: AddressInformation,
        connection_id: u32,
    },
    KeepAlive,
    Embedded(P),
    Acknowledge,
    RequestPrivateAdressInformation(u64),
    PrivateAdressInformation(u64, AddressInformation),
}
