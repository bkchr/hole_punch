use errors::*;
use udp::{self, UdpAcceptStream};
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

pub fn peer_client_main(server_addr: SocketAddr) {
    let mut evt_loop = Core::new().expect("error creating evt loop");
    let handle = evt_loop.handle();
    let listen = ([0, 0, 0, 0], 0);
    let evt_loop_handle = handle.clone();

    let registration = udp::connect_and_accept_async(listen.into(), &handle, 4)
        .then(|r| r.chain_err(|| "error"))
        .and_then(|(server, connect)| {
            let port = server.local_addr().map(|a| a.port()).unwrap();
            connect.connect(server_addr);

            server
                .for_each(move |(stream, addr, stype)| {
                    match stype {
                        udp::StreamType::Connect if addr == server_addr => {
                            let connect = connect.clone();
                            // we work with a length delimited stream
                            let length_delimited = length_delimited::Framed::new(stream);
                            let (writer, reader) = length_delimited.split();

                            let json_writer = WriteBincode::new(writer);
                            let json_reader = ReadBincode::<_, protocol::Protocol>::new(reader);
                            let timer = tokio_timer::wheel().build();

                            evt_loop_handle.spawn(
                                json_writer
                                    .send_all(
                                        futures::stream::once(
                                            Ok(protocol::Protocol::Registration {
                                                name: "peer_client".to_string(),
                                                private: protocol::AddressInformation {
                                                    addresses: interfaces()
                                                        .iter()
                                                        .map(|v| v.ips.clone())
                                                        .concat()
                                                        .iter()
                                                        .map(|v| v.ip())
                                                        .filter(|ip| !ip.is_loopback())
                                                        .collect_vec(),
                                                    port,
                                                },
                                            }),
                                        ).select(
                                            timer
                                                .interval_range(
                                                    Duration::from_secs(30),
                                                    Duration::from_secs(45),
                                                )
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
                                                            for addr in public.addresses {
                                                                connect.connect(
                                                                    (addr, public.port).into(),
                                                                );
                                                            }
                                                            for addr in private.addresses {
                                                                connect.connect(
                                                                    (addr, private.port).into(),
                                                                );
                                                            }

                                                            Some(protocol::Protocol::ConnectionInformation {})
                                                        }
                                                        _ => None,
                                                    }
                                                    })
                                                    .then(|r| r.chain_err(|| "error")),
                                            )
                                            .map_err(
                                                |_| io::Error::new(io::ErrorKind::Other, "other"),
                                            ),
                                    )
                                    .map_err(|_| ())
                                    .map(|_| ()),
                            );
                        }
                        _ => {
                            let timer = tokio_timer::wheel().build();
                            let (swriter, sreader) = stream.split();

                            evt_loop_handle.spawn(
                                swriter
                                    .send_all(
                                        timer
                                            .interval(Duration::from_millis(500))
                                            .map(|_| "hello".to_string().as_bytes().to_vec())
                                            .map_err(|_| panic!("ERROR")),
                                    )
                                    .map(|_| ())
                                    .map_err(|_| ()),
                            );

                            evt_loop_handle.spawn(sreader.for_each(|d| {
                                let d = String::from_utf8(d).unwrap();
                                println!("GOT2: {}", d);
                                Ok(())
                            }));
                        }
                    };

                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        });

    evt_loop.run(registration).expect("error registering");
}
