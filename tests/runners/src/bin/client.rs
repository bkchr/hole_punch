#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate timebomb;
extern crate tokio_core;

use runners::protocol::Protocol;

use hole_punch::{FileFormat, Config, Context, Error, Stream};

use tokio_core::reactor::Core;

use std::net::ToSocketAddrs;

use futures::future::Either;
use futures::{Future, Poll, Stream as FStream};
use futures::Async::Ready;

use structopt::StructOpt;

struct SendAndRecvMessage {
    con: Stream<Protocol>,
    msg_data: String,
}

impl SendAndRecvMessage {
    fn new(mut con: Stream<Protocol>, data: String) -> SendAndRecvMessage {
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
            config.set_cert_chain(vec![ cert.to_vec() ], FileFormat::PEM);
            config.set_key(key.to_vec(), FileFormat::PEM);

            let mut context =
                Context::new(evt_loop.handle(), config).expect("Create hole-punch Context");

            println!("Connecting to server: {}", server_addr);
            let mut server_con = evt_loop
                .run(context.create_connection_to_server(&server_addr))
                .expect("Create connection to server");
            server_con.upgrade_to_authenticated();
            server_con.send_and_poll(Protocol::Register("client".into()));
            println!("Connected");

            let connection_id = context.generate_connection_id();
            let peer_con_req = context
                .create_connection_to_peer(
                    connection_id,
                    &mut server_con,
                    Protocol::RequestPeer("peer".into(), connection_id),
                )
                .expect("Create connection to peer future");

            evt_loop.handle().spawn(
                server_con
                    .into_future()
                    .map_err(|e| panic!("{:?}", e.0))
                    .map(|v| panic!("{:?}", v.0)),
            );

            // TODO: Remove this complicated thing
            // TODO: Handle PeerNotFound
            let ( peer_con, context ) = match evt_loop
                .run(peer_con_req.select2(context.into_future()))
                .map_err(|e| match e {
                    Either::A((e, _)) => panic!(e),
                    Either::B((e, _)) => panic!(e.0),
                })
                .unwrap()
            {
                Either::A((con, context)) => ( con, context ),
                Either::B(_) => {
                    panic!("connection to server closed while waiting for connection to peer")
                }
            };

            // Check that it is a p2p connection, if that was requested
            if options.expect_p2p_connection {
                assert!(peer_con.is_p2p());
            }

            println!("PEER CONNECTED");
            // Check that we actually can send messages
            evt_loop
                .run(SendAndRecvMessage::new(peer_con, "herp and derp".into()))
                .expect("Send and receives message");
        },
        30 * 1000,
    );

    println!("Client finished successfully!");
}
