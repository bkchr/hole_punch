extern crate hole_punch;
extern crate tokio_core;

use hole_punch::server::{NewService, Service, Server, ServiceId};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

enum CarrierProtocol {
    Register { name: String },
    Registered,
}

struct Carrier {
    devices: HashMap<String, ServiceId>;
}

struct CarrierService {
    id: ServiceId,
    carrier: Arc<Mutex<Carrier>>,
    name: String,
}

impl Service for CarrierService {
    fn on_message(&mut self, msg: &CarrierProtocol) -> Result<Option<CarrierProtocol>> {
        match *msg {
            CarrierProtocol::Register {name} => {
                self.name = name.clone();
                println!("New device: {}", name);
                self.carrier.lock().unwrap().insert(name, self.id);
                Ok(Some(CarrierProtocol::Registered()))
            },
            _ => { Ok(None) }
        }
    }

    fn close(&self) {
        self.carrier.lock().unwrap().remove(&self.name);
        println!("device gone {}", self.name);
    }
}

struct CarrierServiceCreator {
    carrier: Arc<Mutex<Carrier>>
}

impl NewService for CarrierServiceCreator {
    fn new_service(&self, id: ServiceId) -> CarrierService {
        CarrierService {
            carrier: self.carrier.clone(),
            id: id,
            name: String::new(),
        }
    }
}

fn main() {
    let evt_loop = Core::new().unwrap();

    let carrier = Arc::new(Mutex::new(Carrier { devices: HashMap::new() }));
    let new_service = CarrierServiceCreator { carrier };

    let server = Server::new(new_service, evt_loop.handle());
    server.run(&evt_loop).expect("server running");
}
