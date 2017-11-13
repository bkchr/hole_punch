use errors::*;
use udp::{self, UdpAcceptStream};
use protocol;

use std::net::SocketAddr;
use std::time::Duration;
use std::io;
use std::io::BufReader;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_io::io as tio;
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_timer;

use futures::{self, Future, Poll, Sink, Stream};
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::Async::{NotReady, Ready};

struct UdpStuff {
    server: udp::UdpServer,
    recv: Receiver<SocketAddr>,
}

impl UdpStuff {
    fn new(server: udp::UdpServer, recv: Receiver<SocketAddr>) -> UdpStuff {
        UdpStuff { server, recv }
    }
}

impl Stream for UdpStuff {
    type Item = (UdpAcceptStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.recv.poll() {
            Ok(Ready(Some(addr))) => Ok(Ready(Some((self.server.connect(addr), addr)))),
            _ => self.server.poll(),
        }
    }
}

pub fn peer_client_main(server: SocketAddr) {
    let mut evt_loop = Core::new().expect("error creating evt loop");
    let handle = evt_loop.handle();
    let listen = ([0, 0, 0, 0], 22223);
    let evt_loop_handle = handle.clone();

    let registration = udp::connect_and_accept_async(server, listen.into(), &handle, 4).and_then(
        |(accept, connect)| {
            // we work with a length delimited stream
            let length_delimited = length_delimited::Framed::new(connect);
            let (writer, reader) = length_delimited.split();

            let json_writer = WriteJson::new(writer);
            let json_reader = ReadJson::<_, protocol::Protocol>::new(reader);
            let timer = tokio_timer::wheel().build();

            let (csender, crecv) = channel(10);
            let evt_loop_handle2 = evt_loop_handle.clone();

            evt_loop_handle.spawn(
                UdpStuff::new(accept, crecv)
                    .for_each(move |c| {
                        evt_loop_handle2.spawn(
                            tio::write_all(c.0, b"hello\n")
                                .and_then(|c| {
                                    let reader = BufReader::new(c.0);
                                    tio::read_until(reader, b'\n', Vec::new()).map(|d| {
                                        let d = String::from_utf8(d.1).unwrap();
                                        println!("{}", d);
                                        ()
                                    })
                                })
                                .map_err(|_| ()),
                        );
                        println!("DATA: {:?}", c.1);
                        Ok(())
                    })
                    .map_err(|_| ()),
            );

            // send our request and after that, wait for the answer
            json_writer
                .send_all(
                    futures::stream::once(Ok(protocol::Protocol::Registration {
                        name: "peer_client".to_string(),
                        private: protocol::AddressInformation {
                            addresses: vec![[127, 0, 0, 1].into()],
                            port: 22223,
                        },
                    })).select(
                        timer
                            .interval_range(Duration::from_secs(30), Duration::from_secs(45))
                            .map(|_| protocol::Protocol::KeepAlive {})
                            .then(|r| r.chain_err(|| "error")),
                    )
                        .select(
                            json_reader
                                .filter_map(move |i| {
                                    println!("{:?}", i);
                                    match i {
                                        protocol::Protocol::RequestConnection {
                                            public,
                                            private,
                                            connection_id,
                                        } => {
                                            let mut csender = csender.clone();
                                            for addr in public.addresses {
                                                csender.start_send((addr, public.port).into());
                                            }
                                            for addr in private.addresses {
                                                csender.start_send((addr, public.port).into());
                                            }
                                            csender.poll_complete();

                                            Some(protocol::Protocol::ConnectionInformation {})
                                        }
                                        _ => None,
                                    }
                                })
                                .then(|r| r.chain_err(|| "error")),
                        )
                        .map_err(|_| io::Error::new(io::ErrorKind::Other, "other")),
                )
                .then(|r| r.chain_err(|| "error sending request"))
        },
    );

    evt_loop.run(registration).expect("error registering");
}
