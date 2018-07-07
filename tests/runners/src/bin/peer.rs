#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate ox;
extern crate timebomb;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_serde_json;

use runners::protocol::Protocol;

use hole_punch::{Config, Context, Error, FileFormat, PubKeyHash};

use tokio_core::reactor::Core;

use std::{net::ToSocketAddrs, thread, time::Duration};

use futures::Async::Ready;
use futures::{Future, Poll, Sink, Stream as FStream};

use structopt::StructOpt;

use tokio_serde_json::{ReadJson, WriteJson};

use tokio_io::codec::length_delimited;

use ox::{generate_fixed_x25519, Certificate, Secret};

type Stream = WriteJson<ReadJson<length_delimited::Framed<hole_punch::Stream>, Protocol>, Protocol>;

fn into_stream(stream: hole_punch::Stream) -> Stream {
    WriteJson::new(ReadJson::new(length_delimited::Framed::new(stream)))
}

struct SendAndRecvMessage {
    stream: Stream,
    msg_data: String,
}

impl SendAndRecvMessage {
    fn new(mut stream: Stream, data: String) -> SendAndRecvMessage {
        stream
            .start_send(Protocol::SendMessage(data.clone()))
            .expect("Sends data");
        stream.poll_complete().unwrap();
        SendAndRecvMessage {
            stream,
            msg_data: data,
        }
    }
}

impl Future for SendAndRecvMessage {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let msg = try_ready!(self.stream.poll()).expect("Receive one message");

        match msg {
            Protocol::ReceiveMessage(data) => {
                assert_eq!(data, self.msg_data);
                return Ok(Ready(()));
            }
            _ => panic!("Received unknown message"),
        }
    }
}

struct RecvAndSendMessage {
    stream: Stream,
}

impl RecvAndSendMessage {
    fn new(stream: Stream) -> RecvAndSendMessage {
        RecvAndSendMessage { stream }
    }
}

impl Future for RecvAndSendMessage {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => {
                    println!("END");
                    return Ok(Ready(()));
                }
            };

            match msg {
                Protocol::SendMessage(data) => {
                    println!("Received: {}", data);
                    self.stream.start_send(Protocol::ReceiveMessage(data))?;
                    self.stream.poll_complete()?;
                }
                _ => panic!("Received unknown message"),
            }
        }
    }
}

#[derive(StructOpt)]
struct Options {
    #[structopt(short = "r", long = "remote_peer")]
    remote_peer: Option<String>,

    #[structopt(long = "peer_id")]
    peer_id: usize,

    #[structopt(long = "expect_p2p_connection")]
    expect_p2p_connection: bool,

    #[structopt(long = "expect_proxy_connection")]
    expect_proxy_connection: bool,

    #[structopt(long = "timeout")]
    timeout: u32,

    #[structopt(long = "request_peer")]
    request_peer: Option<usize>,

    #[structopt(long = "expect_connection")]
    expect_connection: bool,

    #[structopt(long = "listen_port")]
    listen_port: Option<u16>,
}

// Check that the stream is as expected
fn check_stream(stream: &Stream, options: &Options) {
    // Check that it is a p2p connection, if that was expected.
    if options.expect_p2p_connection {
        assert!(stream.get_ref().get_ref().get_ref().is_p2p());
    }

    // Check that the server relays this connection, if that was expected.
    if options.expect_proxy_connection {
        assert!(!stream.get_ref().get_ref().get_ref().is_p2p());
    }
}

fn main() {
    let options = Options::from_args();
    let timeout = options.timeout * 1000;

    let mut secrets = [
        [
            0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c,
            0xae, 0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92,
            0xec, 0x2c, 0xf0, 0x0d,
        ],
        [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0xf0, 0x0d,
        ],
        [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xf0, 0x0d, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b,
            0xac, 0x03, 0x1c, 0xae,
        ],
    ];

    timebomb::timeout_ms(
        move || {
            let peer_id = options.peer_id;
            let remote_peer = options.remote_peer.clone().map(|v| {
                v.to_socket_addrs()
                    .expect("Parses remote peer address")
                    .next()
                    .expect("Resolves remote peer address to ip address")
            });

            let mut evt_loop = Core::new().unwrap();

            let secret = Secret::from_bytes(secrets.get_mut(peer_id).unwrap());
            let identity = secret.identity();

            let (x25519_secret, x25519_public) = generate_fixed_x25519(peer_id as u8);
            let certificate = Certificate::new()
                .identity(&identity)
                .bind_x25519(&x25519_public)
                .sign(&secret)
                .unwrap();

            let mut config_builder = Config::builder()
                .set_shitty_udp_certificate(certificate)
                .set_shitty_udp_private_key(x25519_secret);

            if let Some(remote_peer) = remote_peer {
                config_builder = config_builder.add_remote_peer(remote_peer).unwrap();
            }

            if let Some(listen_port) = options.listen_port {
                config_builder = config_builder.set_shitty_udp_listen_port(listen_port);
            }

            let config = config_builder.build().unwrap();

            let key = PubKeyHash::from_public_key_der(identity.public_key().to_vec(), true)
                .expect("Creates local identifier");
            println!("Creates hole_punch Context: {}", key);
            let context =
                Context::new(key, evt_loop.handle(), config).expect("Create hole-punch Context");

            if let Some(request_peer) = options.request_peer {
                let peer_identity = secret.identity();
                let peer_key =
                    PubKeyHash::from_public_key_der(peer_identity.public_key().to_vec(), true)
                        .expect("Create peer identifier");

                println!("REQUEST: {}", peer_key);

                for _ in 0..3 {
                    let res = evt_loop.run(context.create_connection_to_peer_with_custom_timeout(
                        peer_key.clone(),
                        Duration::from_secs(15),
                    ));

                    let peer_con = match res {
                        Ok(p) => p,
                        Err(e) => match e {
                            Error::PeerNotFound(_) => {
                                // Sleep and give the Peer some time to connect.
                                thread::sleep(Duration::from_secs(5));
                                continue;
                            }
                            e @ _ => panic!(e),
                        },
                    };

                    println!("Created connection to peer{}", request_peer);

                    let mut peer_handle = peer_con.new_stream_handle().clone();
                    let peer_con: Stream = into_stream(peer_con);

                    check_stream(&peer_con, &options);

                    // Check that we actually can send messages
                    evt_loop
                        .run(SendAndRecvMessage::new(peer_con, "herp and derp".into()))
                        .expect("Send and receives message");

                    println!("Creates new Stream");

                    // Check that we can create a new Stream
                    let stream = evt_loop
                        .run(peer_handle.new_stream())
                        .expect("Creates new Stream");
                    let stream: Stream = into_stream(stream);

                    check_stream(&stream, &options);

                    // Check that we actually can send messages
                    evt_loop
                        .run(SendAndRecvMessage::new(stream, "herp and derp2".into()))
                        .expect("Send and receives message 2");
                    return;
                }

                panic!("Could not connect to requested peer!");
            } else {
                let handle = evt_loop.handle();
                println!("Running the Context");

                evt_loop
                    .run(context.for_each(|s| {
                        println!("New Stream from Context");

                        if options.expect_connection {
                            let stream = into_stream(s);
                            check_stream(&stream, &options);

                            handle.spawn(
                                RecvAndSendMessage::new(stream)
                                    .map_err(|e| panic!("RecvAndSendMessage error: {:?}", e)),
                            );
                        } else {
                            panic!("Context returned stream, while none was expected!");
                        };
                        Ok(())
                    }))
                    .expect("Runs context");
            }
        },
        timeout,
    );

    println!("Client finished successfully!");
}
