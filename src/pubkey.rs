use error::*;

use std::{fmt, result, hash::{Hash, Hasher as StdHasher}, ops::Deref};

use openssl::error::ErrorStack;
use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::{PKey, Public};
use openssl::x509::X509;

use openssl_sys;

use hex;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

use bytes::Bytes;

/// A public key.
/// We store the public key as `sha256` hash. If original public key is available at
/// construction and on user request, the original public key is stored in `DER` format.
#[derive(Clone)]
pub struct PubKey {
    /// The hashed public key.
    buf: [u8; openssl_sys::EVP_MAX_MD_SIZE as usize],
    /// The length of the hash.
    len: usize,
    /// The complete public key in DER format.
    /// This value will only be set, if requested. It will also not be serialized.
    orig: Option<Bytes>,
}

impl Hash for PubKey {
    fn hash<H: StdHasher>(&self, state: &mut H) {
        (&self.buf).hash(state);
    }
}

impl Serialize for PubKey {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.buf[..self.len])
    }
}

impl<'de> Deserialize<'de> for PubKey {
    fn deserialize<D>(deserializer: D) -> result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec = Vec::<u8>::deserialize(deserializer)?;

        if vec.len() > openssl_sys::EVP_MAX_MD_SIZE as usize {
            Err(D::Error::invalid_length(vec.len(), &"buf is too long"))
        } else {
            let mut buf = [0; openssl_sys::EVP_MAX_MD_SIZE as usize];
            buf[..vec.len()].copy_from_slice(&vec);

            Ok(PubKey {
                buf,
                len: buf.len(),
                orig: None,
            })
        }
    }
}

impl Deref for PubKey {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[..self.len]
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
    /// Create the public key from a openssl `PKey<Public>`.
    /// If `store_orig` is set, the public key will be stored internally.
    pub fn from_pkey(
        orig_key: PKey<Public>,
        store_orig: bool,
    ) -> result::Result<PubKey, ErrorStack> {
        let mut hasher = Hasher::new(MessageDigest::sha256())?;
        hasher.update(&orig_key.public_key_to_der()?)?;
        let bytes = hasher.finish()?;

        let mut key = Self::from_hashed_checked(&bytes);

        if store_orig {
            key.orig = Some(Bytes::from(orig_key.public_key_to_der()?));
        }

        Ok(key)
    }

    /// Construct the public key from a public key hash.
    /// The function does not checks, if the hash is `sha256`!
    pub fn from_hashed(hashed: &[u8]) -> Result<PubKey> {
        if hashed.len() > openssl_sys::EVP_MAX_MD_SIZE as usize {
            bail!("Size is too long for a hashed value!");
        }

        Ok(Self::from_hashed_checked(hashed))
    }

    /// Constructs the public key from a public key hash in hex format.
    /// The function does not checks, if the hash is `sha256`!
    pub fn from_hashed_hex(hashed: &str) -> Result<PubKey> {
        let buf = hex::decode(hashed)?;

        Self::from_hashed(&buf)
    }

    /// Constructs the public key from a `x509` certificate.
    /// If `store_orig` is set, the public key will be stored internally.
    pub fn from_x509_pem(cert: &[u8], store_orig: bool) -> Result<PubKey> {
        let cert = X509::from_pem(cert)?;

        Ok(Self::from_pkey(cert.public_key()?, store_orig)?)
    }

    /// The hash length is checked and we can safely construct the public key.
    fn from_hashed_checked(hashed: &[u8]) -> PubKey {
        let mut buf = [0; openssl_sys::EVP_MAX_MD_SIZE as usize];
        buf[..hashed.len()].copy_from_slice(hashed);

        PubKey {
            buf,
            len: hashed.len(),
            orig: None,
        }
    }

    /// Returns the original public key, if it was stored.
    pub fn orig_public_key(&self) -> Result<Option<PKey<Public>>> {
        match self.orig {
            Some(ref data) => Ok(Some(PKey::<Public>::public_key_from_der(&data)?)),
            None => Ok(None),
        }
    }

    /// Returns the original public key in `DER` format, if it was stored.
    pub fn orig_public_key_der(&self) -> Option<&[u8]> {
        self.orig.as_ref().map(|v| v.as_ref())
    }
}
