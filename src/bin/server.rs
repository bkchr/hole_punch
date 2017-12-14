extern crate hole_punch;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::server::{NewService, Server, Service, ServiceControlMessage, ServiceId};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Deserialize, Serialize)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
    RequestDevice { name: String },
    DeviceNotFound,
    AlreadyConnected,
}

struct Carrier {
    devices: HashMap<String, ServiceId>,
}

struct CarrierService {
    id: ServiceId,
    carrier: Arc<Mutex<Carrier>>,
    name: String,
    control_message: Option<ServiceControlMessage>,
}

impl Service for CarrierService {
    type Message = CarrierProtocol;

    fn on_message(&mut self, msg: &CarrierProtocol) -> Result<Option<CarrierProtocol>> {
        match msg {
            &CarrierProtocol::Register { ref name } => {
                self.name = name.clone();
                println!("New device: {}", name);
                self.carrier
                    .lock()
                    .unwrap()
                    .devices
                    .insert(name.clone(), self.id);
                Ok(Some(CarrierProtocol::Registered))
            }
            &CarrierProtocol::RequestDevice { ref name } => {
                println!("REQUEST: {}", name);
                let carrier = self.carrier.lock().unwrap();
                let other = carrier.devices.get(name);

                if let Some(id) = other {
                    self.control_message = Some(ServiceControlMessage::CreateConnectionTo(*id));
                    Ok(None)
                } else {
                    Ok(Some(CarrierProtocol::DeviceNotFound))
                }
            }
            _ => Ok(None),
        }
    }

    fn close(&self) {
        self.carrier.lock().unwrap().devices.remove(&self.name);
        println!("device gone {}", self.name);
    }

    fn request_control_message(&mut self) -> Option<ServiceControlMessage> {
        self.control_message.take()
    }
}

struct CarrierServiceCreator {
    carrier: Arc<Mutex<Carrier>>,
}

impl NewService for CarrierServiceCreator {
    type Service = CarrierService;

    fn new_service(&self, id: ServiceId) -> Self::Service {
        CarrierService {
            carrier: self.carrier.clone(),
            id: id,
            name: String::new(),
            control_message: None,
        }
    }
}

fn main() {
    let mut evt_loop = Core::new().unwrap();

    let carrier = Arc::new(Mutex::new(Carrier {
        devices: HashMap::new(),
    }));
    let new_service = CarrierServiceCreator { carrier };

    let server = Server::new(new_service, evt_loop.handle()).expect("server");
    server.run(&mut evt_loop).expect("server running");
}
