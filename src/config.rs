use error::*;

use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

use picoquic::{self, FileFormat};

pub struct ConfigBuilder {
    /// The address where picoquic should listen on.
    /// Default: `0.0.0.0:0`
    quic_listen_address: SocketAddr,
    /// The configuration used for picoquic.
    quic_config: picoquic::Config,
    /// The list of certificate authorities certificates for clients.
    incoming_ca_certificates: Option<Vec<PathBuf>>,
    /// The list of certificate authorities certificates for servers.
    outgoing_ca_certificates: Option<Vec<PathBuf>>,
    /// The list of known remote peers.
    remote_peers: Vec<SocketAddr>,
}

impl ConfigBuilder {
    /// Creates a new `Config` instance.
    fn new() -> ConfigBuilder {
        ConfigBuilder {
            quic_listen_address: ([0, 0, 0, 0], 0).into(),
            quic_config: picoquic::Config::new(),
            incoming_ca_certificates: None,
            outgoing_ca_certificates: None,
            remote_peers: Vec::new(),
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
        self.quic_config.set_key(key, format);
    }

    /// Sets a list of certificate authorities certificates (in PEM format). These
    /// certificates are used to authenticate incoming connections. If no certificates are given,
    /// all incoming connections are successfully authenticated.
    pub fn set_incoming_ca_certificates(&mut self, certs: Vec<PathBuf>) {
        self.incoming_ca_certificates = Some(certs);
    }

    /// Sets a list of certificate authorities certificates (in PEM format). These
    /// certificates are used to authenticate outgoing connections. If no certificates are given,
    /// all outgoing connections are successfully authenticated.
    pub fn set_outgoing_ca_certificates(&mut self, certs: Vec<PathBuf>) {
        self.outgoing_ca_certificates = Some(certs);
    }

    /// Adds a remote peer. The `Context` will always hold a connection to one of the known remote
    /// peers.
    pub fn add_remote_peer(&mut self, remote_peer: impl ToSocketAddrs) -> Result<()> {
        // TODO: For urls we need some kind of update. E.g. the dns record changes.
        remote_peer
            .to_socket_addrs()?
            .for_each(|a| self.remote_peers.push(a));
        Ok(())
    }

    /// Build the `Config`.
    pub fn build(self) -> Result<Config> {
        if self.quic_config.key.is_none() && self.quic_config.key_filename.is_none() {
            bail!("Private key is required!");
        }

        if self.quic_config.cert_chain.is_none() && self.quic_config.cert_chain_filename.is_none() {
            bail!("Certificate chain is required!");
        }

        Ok(Config {
            quic_listen_address: self.quic_listen_address,
            quic_config: self.quic_config,
            incoming_ca_certificates: self.incoming_ca_certificates,
            outgoing_ca_certificates: self.outgoing_ca_certificates,
            remote_peers: self.remote_peers,
        })
    }
}

pub struct Config {
    /// The address where pcioquic should listen on.
    /// Default: `0.0.0.0:0`
    pub(crate) quic_listen_address: SocketAddr,
    /// The configuration used for picoquic.
    pub(crate) quic_config: picoquic::Config,
    /// The list of certificate authorities certificates for incoming connections.
    pub(crate) incoming_ca_certificates: Option<Vec<PathBuf>>,
    /// The list of certificate authorities certificates for outgoing connections.
    pub(crate) outgoing_ca_certificates: Option<Vec<PathBuf>>,
    /// The list of known remote peers.
    pub(crate) remote_peers: Vec<SocketAddr>,
}

impl Config {
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::new()
    }
}
