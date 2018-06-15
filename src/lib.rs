extern crate bincode;
extern crate bytes;
extern crate either;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
extern crate hex;
extern crate itertools;
extern crate log;
extern crate objekt;
extern crate openssl;
extern crate openssl_sys;
extern crate picoquic;
extern crate pnet_datalink;
extern crate rand;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
#[macro_use]
extern crate state_machine_future;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_json;
extern crate tokio_timer;

mod protocol;
#[macro_use]
mod error;
mod authenticator;
mod config;
mod connect;
mod connection;
mod context;
mod incoming_stream;
mod pubkey;
mod registry;
mod strategies;
mod stream;
mod timeout;

pub use authenticator::Authenticator;
pub use config::Config;
pub use connection::ConnectionId;
pub use context::{Context, ResolvePeer, ResolvePeerResult};
pub use error::Error;
pub use picoquic::FileFormat;
pub use pubkey::PubKeyHash;
pub use stream::{Stream, StreamHandle};

pub mod plain {
    pub use strategies::Stream;
}
