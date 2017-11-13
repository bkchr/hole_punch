use errors::*;
use udp;
use protocol;

use std::io;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_serde_json::{ReadJson, WriteJson};

use futures::{Future, Sink, Stream};
use futures::sync::mpsc::{channel, Receiver, Sender};

pub fn server_main() {
    let mut evt_loop = Core::new().expect("could not initialize event loop");

    let evt_loop_handle = evt_loop.handle();
    let evt_loop_handle3 = evt_loop.handle();

    let connections = Arc::new(Mutex::new(HashMap::<
        String,
        (
            protocol::Protocol,
            Sender<(protocol::AddressInformation, protocol::AddressInformation)>,
        ),
    >::new()));
    let connections2 = connections.clone();

    // listen for all incoming requests
    let server = udp::accept_async(([0, 0, 0, 0], 22222).into(), &evt_loop_handle3, 4).and_then(
        move |server| {
            server
                .for_each(move |(con, addr)| {
                    let length_delimited = length_delimited::Framed::new(con);

                    let (writer, reader) = length_delimited.split();

                    let send = WriteJson::new(writer);
                    let recv = ReadJson::<_, protocol::Protocol>::new(reader);

                    let (csender, crecv) = channel(10);

                    let connections = connections.clone();
                    evt_loop_handle.spawn(
                        send.send_all(
                            recv.map(move |v| match v {
                                protocol::Protocol::Registration { name, private } => {
                                    println!("Registered {}", name);
                                    let info = protocol::Protocol::ConnectionInformation2 {
                                        public: protocol::AddressInformation {
                                            addresses: vec![addr.ip()],
                                            port: addr.port(),
                                        },
                                        private,
                                    };
                                    connections
                                        .lock()
                                        .unwrap()
                                        .insert(name, (info, csender.clone()));
                                    protocol::Protocol::KeepAlive {}
                                }
                                _ => protocol::Protocol::KeepAlive {},
                            }).map_err(|_| ())
                                .select(crecv.map(|(public, private)| {
                                    protocol::Protocol::RequestConnection {
                                        public,
                                        private,
                                        connection_id: 1,
                                    }
                                }))
                                .map_err(|_| io::Error::new(io::ErrorKind::Other, "other")),
                        ).map(|_| ())
                            .map_err(|_| ()),
                    );

                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        },
    );

    evt_loop_handle3.spawn(server.map_err(|_| ()));
    let evt_loop_handle = evt_loop_handle3.clone();
    let connections = connections2;

    let server2 = udp::accept_async(([0, 0, 0, 0], 22224).into(), &evt_loop_handle3, 4).and_then(
        move |server| {
            server
                .for_each(move |(con, addr)| {
                    let length_delimited = length_delimited::Framed::new(con);

                    let (writer, reader) = length_delimited.split();

                    let send = WriteJson::new(writer);
                    let recv = ReadJson::<_, protocol::Protocol>::new(reader);

                    let connections = connections.clone();
                    evt_loop_handle.spawn(
                        send.send_all(
                            recv.map(move |v| match v {
                                protocol::Protocol::RequestConnection2 { private, name } => {
                                    println!("REQUEST DEVICE: {}", name);
                                    if let Some(&mut (ref info, ref mut csender)) =
                                        connections.lock().unwrap().get_mut(&name)
                                    {
                                        let public = protocol::AddressInformation {
                                            addresses: vec![addr.ip()],
                                            port: addr.port(),
                                        };
                                        csender.start_send((public, private));
                                        csender.poll_complete();
                                        info.clone()
                                    } else {
                                        protocol::Protocol::DeviceNotExist {}
                                    }
                                }
                                _ => protocol::Protocol::DeviceNotExist {},
                            }).map_err(|_| io::Error::new(io::ErrorKind::Other, "other")),
                        ).map(|_| ())
                            .map_err(|_| ()),
                    );

                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        },
    );

    evt_loop.run(server2).expect("AHHH");
}
