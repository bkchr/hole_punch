use PubKeyHash;
use error::*;
use strategies::GetConnectionId;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::result;
use std::sync::{Arc, Mutex};

use picoquic::{default_verify_certificate, ConnectionId, ConnectionType, VerifyCertificate};

use openssl::error::ErrorStack;
use openssl::stack::StackRef;
use openssl::x509::store::{X509Store, X509StoreBuilder};
use openssl::x509::{X509, X509Ref};

fn create_certificate_store(certs: Option<Vec<PathBuf>>) -> Result<Option<X509Store>> {
    let certs = match certs {
        Some(certs) => certs,
        None => return Ok(None),
    };

    if certs.is_empty() {
        bail!("Certificates list must not be empty!");
    }

    let mut builder = X509StoreBuilder::new()?;

    for cert in certs {
        let mut cert = File::open(cert)?;
        let mut content = Vec::new();

        cert.read_to_end(&mut content)?;
        let cert = X509::from_pem(&content)?;

        builder.add_cert(cert)?;
    }

    Ok(Some(builder.build()))
}

struct Inner {
    ///TODO: we never remove added hashes. That is not good!
    client_pub_keys: HashMap<ConnectionId, PubKeyHash>,
    client_certificates: Option<X509Store>,
    server_certificates: Option<X509Store>,
    store_orig_pub_key: bool,
}

impl Inner {
    fn new(
        server_certs: Option<Vec<PathBuf>>,
        client_certs: Option<Vec<PathBuf>>,
        store_orig_pub_key: bool,
    ) -> Result<Inner> {
        Ok(Inner {
            client_pub_keys: HashMap::new(),
            client_certificates: create_certificate_store(client_certs)?,
            server_certificates: create_certificate_store(server_certs)?,
            store_orig_pub_key,
        })
    }

    fn add_client_pub_key(&mut self, id: ConnectionId, key: PubKeyHash) {
        self.client_pub_keys.insert(id, key);
    }

    fn client_pub_key(&self, id: &ConnectionId) -> Option<PubKeyHash> {
        self.client_pub_keys.get(id).cloned()
    }
}

/// The `Authenticator` is used to authenticate the identities of clients and servers.
/// It will use the certificates specified in the `Config` for the authentication.
///
/// If trusted client certificates are provided in the `Config`, the `Authenticator` stores the
/// public keys of the connected clients. These public keys can be retrieved with `client_pub_key`.
#[derive(Clone)]
pub struct Authenticator {
    inner: Arc<Mutex<Inner>>,
}

impl Authenticator {
    pub(crate) fn new(
        server_certs: Option<Vec<PathBuf>>,
        client_certs: Option<Vec<PathBuf>>,
        store_orig_pub_key: bool,
    ) -> Result<Authenticator> {
        Ok(Authenticator {
            inner: Arc::new(Mutex::new(Inner::new(
                server_certs,
                client_certs,
                store_orig_pub_key,
            )?)),
        })
    }

    /// Returns a public key for a client connection.
    /// This requires client authentication to be activated, or otherwise no public key will be
    /// found for a connection.
    pub fn client_pub_key<C: GetConnectionId>(&mut self, con: &C) -> Option<PubKeyHash> {
        self.inner
            .lock()
            .unwrap()
            .client_pub_key(&con.connection_id())
    }
}

impl VerifyCertificate for Authenticator {
    fn verify(
        &mut self,
        connection_id: ConnectionId,
        connection_type: ConnectionType,
        cert: &X509Ref,
        chain: &StackRef<X509>,
    ) -> result::Result<bool, ErrorStack> {
        let mut inner = self.inner.lock().unwrap();

        match connection_type {
            ConnectionType::Incoming => {
                let res = if let Some(ref store) = (*inner).client_certificates {
                    default_verify_certificate(cert, chain, store)
                } else {
                    panic!("Client authentication activated, but we have no client certificates!")
                };

                if res.is_ok() {
                    let store_orig = (*inner).store_orig_pub_key;
                    inner.add_client_pub_key(
                        connection_id,
                        PubKeyHash::from_pkey(cert.public_key()?, store_orig)?,
                    );
                }

                res
            }
            ConnectionType::Outgoing => {
                if let Some(ref store) = (*inner).server_certificates {
                    default_verify_certificate(cert, chain, store)
                } else {
                    // We are the client and have no trusted certificates for servers, so we trust
                    // any server.
                    Ok(true)
                }
            }
        }
    }
}
