#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate tokio_core;

use runners::protocol::Protocol;

use hole_punch::{Config, Context, Error, Stream};

use tokio_core::reactor::Core;

use std::net::ToSocketAddrs;
use std::env;

use futures::{Future, Poll, Stream as FStream};
use futures::Async::Ready;

use structopt::StructOpt;

struct RecvAndSendMessage {
    con: Stream<Protocol>,
}

impl RecvAndSendMessage {
    fn new(con: Stream<Protocol>) -> RecvAndSendMessage {
        RecvAndSendMessage { con }
    }
}

impl Future for RecvAndSendMessage {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let msg = try_ready!(self.con.poll()).expect("Receive one message");

        match msg {
            Protocol::SendMessage(data) => {
                self.con.send_and_poll(Protocol::ReceiveMessage(data))?;
                return Ok(Ready(()));
            }
            _ => panic!("Received unknown message"),
        }
    }
}

#[derive(StructOpt)]
struct Options {
    #[structopt(short = "s", long = "server_address")]
    server_address: String,
}

fn main() {
    let options = Options::from_args();
    let server_addr = options
        .server_address
        .to_socket_addrs()
        .expect("Parses server address")
        .next()
        .expect("Resolves server address to ip address");

    let mut evt_loop = Core::new().unwrap();

    let mut bin_path = env::current_exe().unwrap();
    bin_path.pop();

    let mut config = Config::new();
    config.set_cert_chain_filename(bin_path.join("cert.pem"));
    config.set_key_filename(bin_path.join("key.pem"));

    let mut context = Context::new(evt_loop.handle(), config).expect("Create hole-punch Context");
    let mut server_con = evt_loop
        .run(context.create_connection_to_server(&server_addr))
        .expect("Create connection to server");
    server_con.upgrade_to_authenticated();
    server_con
        .send_and_poll(Protocol::Register("peer".into()))
        .expect("Registers at server");

    evt_loop.handle().spawn(
        server_con
            .into_future()
            .map_err(|e| panic!(e.0))
            .map(|_| ()),
    );

    let handle = evt_loop.handle();
    evt_loop
        .run(context.for_each(|c| {
            handle.spawn(RecvAndSendMessage::new(c).map_err(|e| panic!(e)));
            Ok(())
        }))
        .expect("Waits for incoming connections");
}
