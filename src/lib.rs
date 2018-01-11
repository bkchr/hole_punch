extern crate bytes;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate futures;
extern crate itertools;
extern crate pnet_datalink;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
#[macro_use]
extern crate state_machine_future;
extern crate tokio_core;
#[macro_use]
extern crate tokio_io;
extern crate tokio_serde_json;
extern crate tokio_timer;

mod protocol;
pub mod server;
pub mod peer_client;
pub mod dev_client;
mod udp;
pub mod errors;
mod strategies;
mod connect;
mod timeout;

pub use strategies::PureConnection;
