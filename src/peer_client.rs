use errors::*;
use udp;
use protocol;

use std::net::SocketAddr;
use std::time::Duration;
use std::io;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_serde_bincode::{ReadBincode, WriteBincode};
use tokio_timer;

use futures::{self, Future, Sink, Stream};

use pnet_datalink::interfaces;

use itertools::Itertools;

pub fn peer_client_main(server_addr: SocketAddr) {}
