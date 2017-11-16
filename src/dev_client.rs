use errors::*;
use udp;
use protocol;

use std::net::SocketAddr;
use std::time::Duration;
use std::io;
use std::io::BufReader;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_io::io as tio;
use tokio_serde_bincode::{ReadBincode, WriteBincode};
use tokio_timer;

use futures::{self, Future, Poll, Sink, Stream};
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::Async::{NotReady, Ready};

use pnet_datalink::interfaces;

use itertools::Itertools;

pub fn dev_client_main() {
    let mut evt_loop = Core::new().expect("error creating evt loop");
    let handle = evt_loop.handle();
    let server = ([176, 9, 73, 99], 22224);
    let evt_loop_handle = handle.clone();

    let registration = udp::connect_async(server.into(), &handle)
        .then(|r| r.chain_err(|| "error"))
        .and_then(|socket| {
            let port = socket.port().unwrap();
            // we work with a length delimited stream
            let length_delimited = length_delimited::Framed::new(socket);
            let (writer, reader) = length_delimited.split();

            let json_writer = WriteBincode::new(writer);
            let json_reader = ReadBincode::<_, protocol::Protocol>::new(reader);

            json_writer
                .send(protocol::Protocol::RequestConnection2 {
                    private: protocol::AddressInformation {
                        addresses: interfaces()
                            .iter()
                            .map(|v| v.ips.clone())
                            .concat()
                            .iter()
                            .map(|v| v.ip())
                            .filter(|ip| !ip.is_loopback())
                            .collect_vec(),
                        port: port,
                    },
                    name: "peer_client".to_string(),
                })
                .then(|r| r.chain_err(|| "error"))
                .and_then(move |_| {
                    json_reader
                        .into_future()
                        .map(|v| v.0)
                        .map_err(|e| e.0)
                        .then(|r| r.chain_err(|| "error"))
                })
                .map(move |v| (port, v))
        });

    let info = evt_loop.run(registration).expect("error registering");
    println!("GOT: {:?}", info);

    let (public, private) = match info.1.unwrap() {
        protocol::Protocol::ConnectionInformation2 { public, private } => (public, private),
        _ => {
            panic!("AHHHH");
        }
    };

    let listen = ([0, 0, 0, 0], info.0);
    let evt_loop_handle = evt_loop.handle();
    let connect = udp::connect_and_accept_async(listen.into(), &handle, 4)
        .then(|r| r.chain_err(|| "error"))
        .and_then(move |(server, connect)| {
            for addr in public
                .addresses
                .iter()
                .map(|v| (*v, public.port))
                .chain(private.addresses.iter().map(|v| (*v, private.port)))
            {
                connect.connect(addr.into());
            }

            server
                .for_each(move |(stream, addr, stype)| {
                    println!("con from: {:?}", addr);

                    match stype {
                        udp::StreamType::Connect => {
                            let timer = tokio_timer::wheel().build();

                            let (swriter, sreader) = stream.split();

                            evt_loop_handle.spawn(
                                swriter
                                    .send_all(
                                        timer
                                            .interval(Duration::from_millis(500))
                                            .map(|_| {
                                                "hello monkeydonkey".to_string().as_bytes().to_vec()
                                            })
                                            .map_err(|_| panic!("ERROR")),
                                    )
                                    .map(|_| ())
                                    .map_err(|_| ()),
                            );

                            evt_loop_handle.spawn(sreader.for_each(move |d| {
                                let d = String::from_utf8(d).unwrap();
                                println!("GOT2: {} {}", d, addr);
                                Ok(())
                            }));
                        }
                        udp::StreamType::Accept => {
                            evt_loop_handle.spawn(
                                tio::write_all(stream, b"hello monkeydonkey\n")
                                    .and_then(move |c| {
                                        let reader = BufReader::new(c.0);
                                        tio::read_until(reader, b'\n', Vec::new()).map(move |d| {
                                            let d = String::from_utf8(d.1).unwrap();
                                            println!("GOT: {}, {:?}", d, addr);
                                            ()
                                        })
                                    })
                                    .map_err(|_| ()),
                            );
                        }
                    };
                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        });

    evt_loop.run(connect).expect("error connect");
}
