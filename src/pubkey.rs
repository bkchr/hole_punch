use error::*;

use std::fmt;
use std::hash::{Hash, Hasher as StdHasher};
use std::result;

use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::{PKey, Public};
use openssl::error::ErrorStack;

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
