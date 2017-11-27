use errors::*;
use udp;
use protocol;
use strategies::{Connection, Strategy};

use std::io;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use tokio_core::reactor::{Core, Handle};
use tokio_io::codec::length_delimited;
use tokio_serde_bincode::{ReadBincode, WriteBincode};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::sync::mpsc::{channel, Receiver, Sender};

use serde::{Deserialize, Serialize};

trait Service {
    type Message;
    fn message(msg: &Self::Message) -> Result<Option<Self::Message>>;
    fn close();
}

type ServiceId = u64;

trait NewService {
    type Service;
    fn new_service(&self, id: ServiceId) -> Self::Service;
}

struct ServiceHandler<T, P>
where
    T: Service,
{
    connection: Connection<P>,
    service: T,
}

impl<T, P> Future for ServiceHandler<T, P>
where
    T: Service,
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let msg = match self.connection.poll()? {
            Ready(Some(msg)) => msg,
            Ready(None) => return Ok(Ready(())),
            NotReady => return Ok(NotReady),
        };


    }
}

struct Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service,
{
    sockets: Vec<Strategy<P>>,
    new_service: N,
    services: HashMap<ServiceId, ServiceHandler<<N as NewService>::Service, P>>,
    unused_ids: Vec<ServiceId>,
}

impl<N, P> Future for Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service,
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        for socket in self.sockets.iter() {
            match socket.poll()? {
                Ready(Some(con)) => {
                    let service_id = self.unused_ids
                        .pop()
                        .unwrap_or_else(|| self.services.len() as u64);
                    let service = self.new_service.new_service(service_id);
                    let handler = ServiceHandler {
                        connection: con.0,
                        service,
                    };

                    self.services.insert(service_id, handler);

                    continue;
                }
                Ready(None) => {
                    bail!("strategy returned None!");
                }
                _ => {}
            }
        }

        Ok(NotReady)
    }
}

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

                    let send = WriteBincode::new(writer);
                    let recv = ReadBincode::<_, protocol::Protocol>::new(reader);

                    let (csender, crecv) = channel(10);

                    let connections = connections.clone();
                    evt_loop_handle.spawn(
                        send.send_all(
                            recv.map(move |v| match v {
                                protocol::Protocol::Register { name, private } => {
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

                    let send = WriteBincode::new(writer);
                    let recv = ReadBincode::<_, protocol::Protocol>::new(reader);

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
