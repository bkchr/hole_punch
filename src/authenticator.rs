use error::*;
use strategies::GetConnectionId;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use std::result;
use std::fs::File;
use std::io::Read;
use std::fmt;
use std::hash::{Hash, Hasher as StdHasher};

use picoquic::{default_verify_certificate, ConnectionId, ConnectionType, VerifyCertificate};

use openssl::error::ErrorStack;
use openssl::x509::{X509, X509Ref};
use openssl::stack::StackRef;
use openssl::x509::store::{X509Store, X509StoreBuilder};
use openssl::pkey::{PKey, Public};
use openssl::hash::{Hasher, MessageDigest};

use openssl_sys;

use hex;

use serde::{Deserialize, Deserializer, Serializer};
use serde::de::Error;

/// A public key.
#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct PubKey {
    #[serde(serialize_with = "serialize_pubkey_array")]
    #[serde(deserialize_with = "deserialize_pubkey_array")]
    buf: [u8; openssl_sys::EVP_MAX_MD_SIZE as usize],
    len: usize,
}

impl Hash for PubKey {
    fn hash<H: StdHasher>(&self, state: &mut H) {
        (&self.buf).hash(state);
    }
}

fn serialize_pubkey_array<S>(buf: &[u8], serializer: S) -> result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_bytes(buf)
}

fn deserialize_pubkey_array<'de, D>(
    deserializer: D,
) -> result::Result<[u8; openssl_sys::EVP_MAX_MD_SIZE as usize], D::Error>
where
    D: Deserializer<'de>,
{
    let vec = Vec::<u8>::deserialize(deserializer)?;

    if vec.len() > openssl_sys::EVP_MAX_MD_SIZE as usize {
        Err(D::Error::invalid_length(vec.len(), &"buf is too long"))
    } else {
        let mut buf = [0; openssl_sys::EVP_MAX_MD_SIZE as usize];
        buf[..vec.len()].copy_from_slice(&vec);
        Ok(buf)
    }
}

impl PartialEq for PubKey {
    fn eq(&self, other: &PubKey) -> bool {
        if self.len == other.len {
            self.buf[..self.len] == other.buf[..other.len]
        } else {
            false
        }
    }
}

impl Eq for PubKey {}

impl fmt::Display for PubKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode_upper(&self.buf[..self.len]))
    }
}

impl PubKey {
    pub fn from_pkey(key: PKey<Public>) -> result::Result<PubKey, ErrorStack> {
        let mut hasher = Hasher::new(MessageDigest::sha256())?;
        hasher.update(&key.public_key_to_der()?)?;
        let bytes = hasher.finish()?;

        Ok(Self::from_checked_hashed(&bytes))
    }

    pub fn from_hashed(hashed: &[u8]) -> Result<PubKey> {
        if hashed.len() > openssl_sys::EVP_MAX_MD_SIZE as usize {
            bail!("Size is too long for a hashed value!");
        }

        Ok(Self::from_checked_hashed(hashed))
    }

    pub fn from_hashed_hex(hashed: &str) -> Result<PubKey> {
        let buf = hex::decode(hashed)?;

        Self::from_hashed(&buf)
    }

    fn from_checked_hashed(hashed: &[u8]) -> PubKey {
        let mut buf = [0; openssl_sys::EVP_MAX_MD_SIZE as usize];
        buf[..hashed.len()].copy_from_slice(hashed);

        PubKey {
            buf,
            len: hashed.len(),
        }
    }
}

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
    client_pub_keys: HashMap<ConnectionId, PubKey>,
    client_certificates: Option<X509Store>,
    server_certificates: Option<X509Store>,
}

impl Inner {
    fn new(
        server_certs: Option<Vec<PathBuf>>,
        client_certs: Option<Vec<PathBuf>>,
    ) -> Result<Inner> {
        Ok(Inner {
            client_pub_keys: HashMap::new(),
            client_certificates: create_certificate_store(client_certs)?,
            server_certificates: create_certificate_store(server_certs)?,
        })
    }

    fn add_client_pub_key(&mut self, id: ConnectionId, key: PubKey) {
        self.client_pub_keys.insert(id, key);
    }

    fn client_pub_key(&self, id: &ConnectionId) -> Option<PubKey> {
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
    ) -> Result<Authenticator> {
        Ok(Authenticator {
            inner: Arc::new(Mutex::new(Inner::new(server_certs, client_certs)?)),
        })
    }

    /// Returns a public key for a client connection.
    /// This requires client authentication to be activated, or otherwise no public key will be
    /// found for a connection.
    pub fn client_pub_key<C: GetConnectionId>(&mut self, con: &C) -> Option<PubKey> {
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
    ) -> result::Result<(), ErrorStack> {
        let mut inner = self.inner.lock().unwrap();

        match connection_type {
            ConnectionType::Incoming => {
                let res = if let Some(ref store) = (*inner).client_certificates {
                    default_verify_certificate(cert, chain, store)
                } else {
                    panic!("Client authentication activated, but we have no client certificates!")
                };

                if res.is_ok() {
                    inner.add_client_pub_key(connection_id, PubKey::from_pkey(cert.public_key()?)?);
                }

                res
            }
            ConnectionType::Outgoing => {
                if let Some(ref store) = (*inner).server_certificates {
                    default_verify_certificate(cert, chain, store)
                } else {
                    // We are the client and have no trusted certificates for servers, so we trust
                    // any server.
                    Ok(())
                }
            }
        }
    }
}
