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

            let json_writer = WriteJson::new(writer);
            let json_reader = ReadJson::<_, protocol::Protocol>::new(reader);

            json_writer
                .send(protocol::Protocol::RequestConnection2 {
                    private: protocol::AddressInformation {
                        addresses: vec![[192, 168, 1, 80].into()],
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
    let connect = udp::connect_and_accept_async(
        (*public.addresses.get(0).unwrap(), public.port).into(),
        listen.into(),
        &handle,
        4,
    ).then(|r| r.chain_err(|| "error"))
        .and_then(move |(server, socket)| {
            let evt_loop_handle2 = evt_loop_handle.clone();
            let timer = tokio_timer::wheel().build();

            let (swriter, sreader) = socket.split();

            evt_loop_handle.spawn(
                swriter
                    .send_all(
                        timer
                            .interval(Duration::from_millis(500))
                            .map(|_| {
                                //println!("SEND");
                                "hello monkeydonkey".to_string().as_bytes().to_vec()
                            })
                            .map_err(|_| panic!("ERROR"))
                    )
                    .map(|_| ())
                    .map_err(|_| ()),
            );

            evt_loop_handle.spawn(sreader.for_each(|d| {
                let d = String::from_utf8(d).unwrap();
                println!("GOT2: {} PUBLIC", d);
                Ok(())
            }));


            server
                .for_each(move |(con, addr)| {
                    println!("con from: {:?}", addr);
                    evt_loop_handle2.spawn(
                        tio::write_all(con, b"hello monkeydonkey\n")
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
                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        });
/*
    evt_loop.handle().spawn(connect.map_err(|_| ()));

    let listen = ([0, 0, 0, 0], info.0);
    let evt_loop_handle = evt_loop.handle();
    let connect = udp::connect_and_accept_async(
        (*private.addresses.get(0).unwrap(), private.port).into(),
        listen.into(),
        &handle,
        4,
    ).then(|r| r.chain_err(|| "error"))
        .and_then(|(server, socket)| {
            let evt_loop_handle2 = evt_loop_handle.clone();
            let timer = tokio_timer::wheel().build();

            let (swriter, sreader) = socket.split();

            evt_loop_handle.spawn(
                swriter
                    .send_all(
                        timer
                            .interval(Duration::from_millis(500))
                            .map(|_| {
                                //println!("SEND");
                                "hello monkeydonkey".to_string().as_bytes().to_vec()
                            })
                            .map_err(|_| panic!("ERROR"))
                    )
                    .map(|_| ())
                    .map_err(|_| ()),
            );

            evt_loop_handle.spawn(sreader.for_each(|d| {
                let d = String::from_utf8(d).unwrap();
                println!("GOT2: {}, PRIVATE", d);
                Ok(())
            }));


            server
                .for_each(move |(con, addr)| {
                    println!("con from: {:?}", addr);
                    evt_loop_handle2.spawn(
                        tio::write_all(con, b"hello monkeydonkey\n")
                            .and_then(|c| {
                                let reader = BufReader::new(c.0);
                                tio::read_until(reader, b'\n', Vec::new()).map(|d| {
                                    let d = String::from_utf8(d.1).unwrap();
                                    println!("GOT: {}", d);
                                    ()
                                })
                            })
                            .map_err(|_| ()),
                    );
                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        });
*/
    evt_loop.run(connect).expect("error connect");
}
