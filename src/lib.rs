extern crate bincode;
extern crate bytes;
extern crate either;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
extern crate hex;
extern crate itertools;
#[macro_use]
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
mod strategies;
mod connect;
mod timeout;
mod config;
mod context;
mod incoming;
mod connection_request;
mod authenticator;
mod pubkey;

pub use error::Error;
pub use context::{ConnectionId, Context, Stream, StreamHandle};
pub use config::Config;
pub use authenticator::Authenticator;
pub use pubkey::PubKey;

pub mod plain {
    pub use strategies::Stream;
}
