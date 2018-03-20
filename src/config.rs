use std::net::SocketAddr;
use std::path::PathBuf;

use picoquic::{self, FileFormat};

pub struct Config {
    /// The address where pcioquic should listen on.
    /// Default: `0.0.0.0:0`
    pub quic_listen_address: SocketAddr,
    /// The configuration used for picoquic.
    pub quic_config: picoquic::Config,
    /// The list of certificate authorities certificates for clients.
    pub client_ca_certificates: Option<Vec<PathBuf>>,
    /// The list of certificate authorities certificates for servers.
    pub server_ca_certificates: Option<Vec<PathBuf>>,
}

impl Config {
    /// Creates a new `Config` instance.
    pub fn new() -> Config {
        Config {
            quic_listen_address: ([0, 0, 0, 0], 0).into(),
            quic_config: picoquic::Config::new(),
            client_ca_certificates: None,
            server_ca_certificates: None,
        }
    }

    /// Sets the listen port for picoquic.
    pub fn set_quic_listen_port(&mut self, port: u16) {
        self.quic_listen_address.set_port(port);
    }

    /// Sets the listen address for picoquic (overwrites `set_quic_listen_port`).
    pub fn set_quic_listen_address(&mut self, addr: SocketAddr) {
        self.quic_listen_address = addr;
    }

    /// Sets the certificate (in PEM format) filename for TLS.
    pub fn set_cert_chain_filename<C: Into<PathBuf>>(&mut self, cert: C) {
        self.quic_config.set_cert_chain_filename(cert);
    }

    /// Sets the key (in PEM format) filename for TLS.
    pub fn set_key_filename<P: Into<PathBuf>>(&mut self, path: P) {
        self.quic_config.set_key_filename(path);
    }

    /// Sets the certificate chain.
    /// This option will overwrite `set_cert_chain_filename`.
    pub fn set_cert_chain(&mut self, certs: Vec<Vec<u8>>, format: FileFormat) {
        self.quic_config.set_cert_chain(certs, format);
    }

    /// Sets the private key.
    /// This option will overwrite `set_key_filename`.
    pub fn set_key(&mut self, key: Vec<u8>, format: FileFormat) {
        self.set_key(key, format);
    }

    /// Sets a list of client certificate authorities certificates (in PEM format). These
    /// certificates are used to authenticate clients. If no certificates are given, all clients
    /// are successfully authenticated.
    pub fn set_client_ca_certificates(&mut self, certs: Vec<PathBuf>) {
        self.client_ca_certificates = Some(certs);
    }

    /// Sets a list of server certificate authorities certificates (in PEM format). These
    /// certificates are used to authenticate servers. If no certificates are given, all servers
    /// are successfully authenticated.
    pub fn set_server_ca_certificates(&mut self, certs: Vec<PathBuf>) {
        self.server_ca_certificates = Some(certs);
    }
}
