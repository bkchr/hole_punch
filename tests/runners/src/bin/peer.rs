#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate tokio_core;

use runners::protocol::Protocol;

use hole_punch::{Config, Context, Error, FileFormat, ResolvePeer, ResolvePeerResult, Stream};

use tokio_core::reactor::Core;

use std::net::ToSocketAddrs;

use futures::Async::Ready;
use futures::{Future, Poll, Stream as FStream};

use structopt::StructOpt;

#[derive(Clone)]
struct DummyResolvePeer {}

impl ResolvePeer<Protocol> for DummyResolvePeer {
    type Identifier = String;
    fn resolve_peer(&self, _: &Self::Identifier) -> ResolvePeerResult<Protocol, Self> {
        ResolvePeerResult::NotFound
    }
}

struct RecvAndSendMessage {
    con: Stream<Protocol, DummyResolvePeer>,
}

impl RecvAndSendMessage {
    fn new(con: Stream<Protocol, DummyResolvePeer>) -> RecvAndSendMessage {
        RecvAndSendMessage { con }
    }
}

impl Future for RecvAndSendMessage {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.con.poll()) {
                Some(msg) => msg,
                None => {
                    println!("END");
                    return Ok(Ready(()));
                }
            };

            match msg {
                Protocol::SendMessage(data) => {
                    println!("Received: {}", data);
                    self.con.send_and_poll(Protocol::ReceiveMessage(data))?;
                }
                _ => panic!("Received unknown message"),
            }
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

    let cert = include_bytes!("../../certs/cert.pem");
    let key = include_bytes!("../../certs/key.pem");

    let mut config = Config::new();
    config.set_cert_chain(vec![cert.to_vec()], FileFormat::PEM);
    config.set_key(key.to_vec(), FileFormat::PEM);

    let mut context = Context::new(
        "peer".into(),
        evt_loop.handle(),
        config,
        DummyResolvePeer {},
    ).expect("Create hole-punch Context");

    println!("Connecting to server: {}", server_addr);
    let mut server_con = evt_loop
        .run(context.create_connection_to_server(&server_addr))
        .expect("Create connection to server");
    server_con.upgrade_to_authenticated();
    server_con
        .send_and_poll(Protocol::Register("peer".into()))
        .expect("Registers at server");
    println!("Connected");

    evt_loop.handle().spawn(
        server_con
            .into_future()
            .map_err(|e| panic!(e.0))
            .map(|v| panic!("{:?}", v.0)),
    );

    let handle = evt_loop.handle();
    evt_loop
        .run(context.for_each(|c| {
            eprintln!("New peer connected");
            handle.spawn(RecvAndSendMessage::new(c).map_err(|e| panic!(e)));
            Ok(())
        }))
        .expect("Waits for incoming connections");
}
