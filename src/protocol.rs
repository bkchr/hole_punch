use std::net::IpAddr;

pub struct Registration {
    name: String,
}

pub struct AddressInformation {
    addresses: Vec<IpAddr>,
    port: u16,
}

pub enum CACRequest {
    RequestConnection {
        public: AddressInformation,
        private: AddressInformation,
    },
}

pub enum CACAnswer {
    ConnectionInformation { private: AddressInformation },
    KeepAlive,
}
