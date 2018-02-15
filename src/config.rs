use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Config {
    /// The address where the udp socket should listen on.
    pub udp_listen_address: SocketAddr,
    /// Path to the cert file.
    pub cert_file: PathBuf,
    /// Path to the key file.
    pub key_file: PathBuf,
}
