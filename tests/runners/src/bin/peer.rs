#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
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

    let certs = [
        include_bytes!("../../certs/peer0_cert.pem").to_vec(),
        include_bytes!("../../certs/peer1_cert.pem").to_vec(),
        include_bytes!("../../certs/peer2_cert.pem").to_vec(),
    ];
    let keys = [
        include_bytes!("../../certs/peer0_key.pem").to_vec(),
        include_bytes!("../../certs/peer1_key.pem").to_vec(),
        include_bytes!("../../certs/peer2_key.pem").to_vec(),
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

            let cert = certs.get(peer_id).unwrap();
            let key = &keys[peer_id];

            let mut config_builder = Config::builder()
                .set_cert_chain(vec![cert.to_vec()], FileFormat::PEM)
                .set_key(key.to_vec(), FileFormat::PEM);

            if let Some(remote_peer) = remote_peer {
                config_builder = config_builder.add_remote_peer(remote_peer).unwrap();
            }

            if let Some(listen_port) = options.listen_port {
                config_builder = config_builder.set_quic_listen_port(listen_port);
            }

            let config = config_builder.build().unwrap();

            println!("Creates hole_punch Context");
            let context = Context::new(
                PubKeyHash::from_x509_pem(cert, true).expect("Creates local identifier"),
                evt_loop.handle(),
                config,
            ).expect("Create hole-punch Context");

            if let Some(request_peer) = options.request_peer {
                let peer_key = PubKeyHash::from_x509_pem(&certs[request_peer], true)
                    .expect("Create peer identifier");

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
