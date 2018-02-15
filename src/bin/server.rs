#[macro_use]
extern crate futures;
extern crate hole_punch;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::{config, context};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::path::PathBuf;

use futures::{Future, Poll, Sink, Stream};
use futures::Async::Ready;

#[derive(Deserialize, Serialize, Clone)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
    RequestDevice { name: String },
    DeviceNotFound,
    AlreadyConnected,
}

struct Carrier {
    devices: HashMap<String, Rc<RefCell<context::Stream<CarrierProtocol>>>>,
}

struct CarrierConnection {
    con: context::Connection<CarrierProtocol>,
    stream: Rc<RefCell<context::Stream<CarrierProtocol>>>,
    name: Option<String>,
    carrier: Rc<RefCell<Carrier>>,
}

impl Future for CarrierConnection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.borrow_mut().poll()) {
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
                        .insert(name, self.stream.clone());

                    self.stream
                        .borrow_mut()
                        .start_send(CarrierProtocol::Registered);
                    self.stream.borrow_mut().poll_complete();
                }
                CarrierProtocol::RequestDevice { name } => {
                    println!("REQUEST: {}", name);

                    self.stream
                        .borrow_mut()
                        .start_send(CarrierProtocol::DeviceNotFound);
                    self.stream.borrow_mut().poll_complete();
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

    let config = config::Config {
        udp_listen_address: ([0, 0, 0, 0], 22222).into(),
        cert_file: PathBuf::from(format!("{}/src/bin/cert.pem", manifest_dir)),
        key_file: PathBuf::from(format!("{}/src/bin/key.pem", manifest_dir)),
    };

    let server = context::Context::new(evt_loop.handle(), config).unwrap();

    let handle = evt_loop.handle();
    let server = server.for_each(|(con, stream)| {
        handle.spawn(
            CarrierConnection {
                con,
                stream: Rc::new(RefCell::new(stream)),
                name: None,
                carrier: carrier.clone(),
            }.map_err(|e| println!("error: {:?}", e)),
        );
        Ok(())
    });

    evt_loop.run(server).unwrap();
}
