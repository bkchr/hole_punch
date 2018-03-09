use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Config {
    /// The address where the udp socket should listen on.
    pub udp_listen_address: SocketAddr,
    /// Path to the cert file.
    pub cert_file: PathBuf,
    /// Path to the key file.
    pub key_file: PathBuf,
    /// A list of certificates which are used to authenticate clients.
    pub trusted_client_certificates: Option<Vec<PathBuf>>,
    /// A list of certificates which are used to authenticate servers.
    pub trusted_server_certificates: Option<Vec<PathBuf>>,
}

impl Config {
    pub fn new<C, K>(udp_listen_address: SocketAddr, cert_file: C, key_file: K) -> Config
    where
        C: Into<PathBuf>,
        K: Into<PathBuf>,
    {
        Config {
            udp_listen_address,
            key_file: key_file.into(),
            cert_file: cert_file.into(),
            trusted_client_certificates: None,
            trusted_server_certificates: None,
        }
    }

    /// Sets a list of trusted client certificates (in PEM format). These certificates are used to
    /// authenticate the identity of a connecting client. This automatically activates the client
    /// authentication in TLS.
    pub fn set_trusted_client_certificates(&mut self, certs: Vec<PathBuf>) {
        self.trusted_client_certificates = Some(certs);
    }

    /// Sets a list of trusted server certificates (in PEM format). These certificates are used to
    /// authenticate the identity of a server.
    pub fn set_trusted_server_certificates(&mut self, certs: Vec<PathBuf>) {
        self.trusted_server_certificates = Some(certs);
    }
}
