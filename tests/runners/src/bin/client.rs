#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate timebomb;
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

struct SendAndRecvMessage {
    con: Stream<Protocol, DummyResolvePeer>,
    msg_data: String,
}

impl SendAndRecvMessage {
    fn new(mut con: Stream<Protocol, DummyResolvePeer>, data: String) -> SendAndRecvMessage {
        con.send_and_poll(Protocol::SendMessage(data.clone()))
            .expect("Sends data");
        SendAndRecvMessage {
            con,
            msg_data: data,
        }
    }
}

impl Future for SendAndRecvMessage {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let msg = try_ready!(self.con.poll()).expect("Receive one message");

        match msg {
            Protocol::ReceiveMessage(data) => {
                assert_eq!(data, self.msg_data);
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

    #[structopt(long = "expect_p2p_connection")]
    expect_p2p_connection: bool,

    #[structopt(long = "expect_relay_connection")]
    expect_relay_connection: bool,
}

// Check that the stream is as expected
fn check_stream(stream: &Stream<Protocol, DummyResolvePeer>, options: &Options) {
    // Check that it is a p2p connection, if that was expected.
    if options.expect_p2p_connection {
        assert!(stream.is_p2p());
    }

    // Check that the server relays this connection, if that was expected.
    if options.expect_relay_connection {
        assert!(!stream.is_p2p());
    }
}

fn main() {
    timebomb::timeout_ms(
        || {
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
                "client".into(),
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
                .send_and_poll(Protocol::Register("client".into()))
                .unwrap();
            println!("Connected");

            let peer_con_req = server_con
                .request_connection_to_peer("peer".into())
                .expect("Create connection to peer future");

            evt_loop.handle().spawn(
                server_con
                    .into_future()
                    .map_err(|e| panic!("{:?}", e.0))
                    .map(|v| panic!("{:?}", v.0)),
            );

            let peer_con = evt_loop
                .run(peer_con_req)
                .expect("Creates connection to peer.");

            check_stream(&peer_con, &options);

            println!("Peer connected");
            let mut peer_handle = peer_con.get_stream_handle();
            // Check that we actually can send messages
            evt_loop
                .run(SendAndRecvMessage::new(peer_con, "herp and derp".into()))
                .expect("Send and receives message");

            println!("Creates new Stream");
            // Check that we can create a new Stream
            let stream = evt_loop.run(peer_handle.new_stream()).expect("Creates new Stream");

            check_stream(&stream, &options);

            // Check that we actually can send messages
            evt_loop
                .run(SendAndRecvMessage::new(stream, "herp and derp2".into()))
                .expect("Send and receives message 2");
        },
        50 * 1000,
    );

    println!("Client finished successfully!");
}
