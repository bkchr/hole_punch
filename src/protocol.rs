use std::net::IpAddr;

#[derive(Serialize, Deserialize, Debug)]
pub struct Registration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddressInformation {
    addresses: Vec<IpAddr>,
    port: u16,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum CACRequest {
    RequestConnection {
        public: AddressInformation,
        private: AddressInformation,
        connection_id: u32,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum CACAnswer {
    ConnectionInformation { private: AddressInformation },
    KeepAlive,
}
