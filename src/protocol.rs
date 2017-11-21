use std::net::IpAddr;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AddressInformation {
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Protocol {
    RequestConnection {
        public: AddressInformation,
        private: AddressInformation,
        connection_id: u32,
    },
    RequestConnection2 {
        private: AddressInformation,
        name: String,
    },
    KeepAlive,
    ConnectionInformation {},
    Register {
        name: String,
        private: AddressInformation,
    },
    ConnectionInformation2 {
        public: AddressInformation,
        private: AddressInformation,
    },
    DeviceNotExist,
}
