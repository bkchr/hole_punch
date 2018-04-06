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

/// A hashed public key.
/// We store the public key as `sha256` hash. If original public key is available at
/// construction and the user requests it, the original public key is also stored in `DER` format.
#[derive(Clone)]
pub struct PubKeyHash {
    /// The hashed public key.
    buf: [u8; openssl_sys::EVP_MAX_MD_SIZE as usize],
    /// The length of the hash.
    len: usize,
    /// The complete public key in DER format.
    /// This value will only be set, if requested. It will also not be serialized.
    pub_key: Option<Bytes>,
}

impl Hash for PubKeyHash {
    fn hash<H: StdHasher>(&self, state: &mut H) {
        (&self.buf).hash(state);
    }
}

impl Serialize for PubKeyHash {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.buf[..self.len])
    }
}

impl<'de> Deserialize<'de> for PubKeyHash {
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

            Ok(PubKeyHash {
                buf,
                len: vec.len(),
                pub_key: None,
            })
        }
    }
}

impl Deref for PubKeyHash {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl PartialEq for PubKeyHash {
    fn eq(&self, other: &PubKeyHash) -> bool {
        if self.len == other.len {
            self.buf[..self.len] == other.buf[..other.len]
        } else {
            false
        }
    }
}

impl Eq for PubKeyHash {}

impl fmt::Display for PubKeyHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode_upper(&self.buf[..self.len]))
    }
}

impl PubKeyHash {
    /// Create the public key hash from a openssl `PKey<Public>`.
    /// If `store_orig` is set, the public key will be stored internally.
    pub fn from_pkey(
        orig_key: PKey<Public>,
        store_orig: bool,
    ) -> result::Result<PubKeyHash, ErrorStack> {
        let mut hasher = Hasher::new(MessageDigest::sha256())?;
        hasher.update(&orig_key.public_key_to_der()?)?;
        let bytes = hasher.finish()?;

        let mut key = Self::from_hashed_checked(&bytes);

        if store_orig {
            key.pub_key = Some(Bytes::from(orig_key.public_key_to_der()?));
        }

        Ok(key)
    }

    /// Construct the public key hash from a public key hash.
    /// The function does not checks, if the hash is `sha256`!
    pub fn from_hashed(hashed: &[u8]) -> Result<PubKeyHash> {
        if hashed.len() > openssl_sys::EVP_MAX_MD_SIZE as usize {
            bail!("Size is too long for a hashed value!");
        }

        Ok(Self::from_hashed_checked(hashed))
    }

    /// Constructs the public key hash from a public key hash in hex format.
    /// The function does not checks, if the hash is `sha256`!
    pub fn from_hashed_hex(hashed: &str) -> Result<PubKeyHash> {
        let buf = hex::decode(hashed)?;

        Self::from_hashed(&buf)
    }

    /// Constructs the public key hash from a `x509` certificate.
    /// If `store_orig` is set, the public key will be stored internally.
    pub fn from_x509_pem(cert: &[u8], store_orig: bool) -> Result<PubKeyHash> {
        let cert = X509::from_pem(cert)?;

        Ok(Self::from_pkey(cert.public_key()?, store_orig)?)
    }

    /// The hash length is checked and we can safely construct the public key hash.
    fn from_hashed_checked(hashed: &[u8]) -> PubKeyHash {
        let mut buf = [0; openssl_sys::EVP_MAX_MD_SIZE as usize];
        buf[..hashed.len()].copy_from_slice(hashed);

        PubKeyHash {
            buf,
            len: hashed.len(),
            pub_key: None,
        }
    }

    /// Returns the public key, if it was stored.
    pub fn public_key(&self) -> Result<Option<PKey<Public>>> {
        match self.pub_key {
            Some(ref data) => Ok(Some(PKey::<Public>::public_key_from_der(&data)?)),
            None => Ok(None),
        }
    }

    /// Returns the public key in `DER` format, if it was stored.
    pub fn public_key_der(&self) -> Option<&[u8]> {
        self.pub_key.as_ref().map(|v| v.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn serialize_deserialize_pubkey_hash() {
        let hash = PubKeyHash::from_hashed_hex(
            "A9106DC0EE32264A045CF96B15DD25069B2A112FA5A464DA2F4F9FE5755DA23D",
        ).unwrap();

        let json = to_string(&hash).unwrap();
        let des_hash: PubKeyHash = from_str(&json).unwrap();

        assert!(hash == des_hash);
    }
}
