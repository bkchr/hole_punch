#[macro_use]
extern crate futures;
extern crate hole_punch;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::{Config, Stream, StreamHandle, Context, Error, ConnectionId};

use tokio_core::reactor::Core;

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::path::PathBuf;

use futures::{Future, Poll, Sink, Stream as FStream};
use futures::Async::Ready;

#[derive(Deserialize, Serialize, Clone)]
enum CarrierProtocol {
    Register {
        name: String,
    },
    Registered,
    RequestDevice {
        name: String,
        connection_id: ConnectionId,
    },
    DeviceNotFound,
    AlreadyConnected,
}

struct Carrier {
    devices: HashMap<String, StreamHandle<CarrierProtocol>>,
}

struct CarrierConnection {
    stream: Stream<CarrierProtocol>,
    name: Option<String>,
    carrier: Rc<RefCell<Carrier>>,
}

impl Future for CarrierConnection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => {
                    println!("stream closed: {:?}", self.name);
                    if let Some(ref name) = self.name {
                        self.carrier.borrow_mut().devices.remove(name);
                    }
                    return Ok(Ready(()));
                }
            };

            match msg {
                CarrierProtocol::Register { name } => {
                    self.name = Some(name.clone());
                    println!("New device: {}", name);
                    self.carrier
                        .borrow_mut()
                        .devices
                        .insert(name, self.stream.get_stream_handle());
                    self.stream.upgrade_to_authenticated();

                    self.stream
                        .start_send(CarrierProtocol::Registered);
                    self.stream.poll_complete();
                }
                CarrierProtocol::RequestDevice {
                    name,
                    connection_id,
                } => {
                    println!("REQUEST: {} {}", name, connection_id);

                    if let Some(mut handle) = self.carrier.borrow_mut().devices.get_mut(&name) {
                        self.stream
                            .create_connection_to(connection_id, &mut handle)?;
                    } else {
                        self.stream
                            .start_send(CarrierProtocol::DeviceNotFound);
                        self.stream.poll_complete();
                    }
                }
                _ => {}
            };
        }
    }
}

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut evt_loop = Core::new().unwrap();

    let carrier = Rc::new(RefCell::new(Carrier {
        devices: HashMap::new(),
    }));

    let config = Config {
        udp_listen_address: ([0, 0, 0, 0], 22222).into(),
        cert_file: PathBuf::from(format!("{}/src/bin/cert.pem", manifest_dir)),
        key_file: PathBuf::from(format!("{}/src/bin/key.pem", manifest_dir)),
    };

    let server = Context::new(evt_loop.handle(), config).unwrap();

    let handle = evt_loop.handle();
    let server = server.for_each(|stream| {
        handle.spawn(
            CarrierConnection {
                stream: stream,
                name: None,
                carrier: carrier.clone(),
            }.map_err(|e| println!("error: {:?}", e)),
        );
        Ok(())
    });

    evt_loop.run(server).unwrap();
}
