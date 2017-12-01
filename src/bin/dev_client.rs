extern crate hole_punch;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::dev_client::{Client, Service};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::net::SocketAddr;

#[derive(Deserialize, Serialize)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
}

struct CarrierService {
    
}

impl Service for CarrierService {
    type Message = CarrierProtocol;

    fn new_connection(&mut self, addr: SocketAddr) -> Option<Self::Message> {
        println!("Yeah, new connection, {:?}", addr);
        Some(CarrierProtocol::Register { name: "nice".to_string() })
    }

    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>> {
        match msg {
            &CarrierProtocol::Registered => {
                println!("REGISTERED");
                Ok(None)
            },
            _ => {
                Ok(None)
            }
        }
    }

    fn connect_to(&self) -> SocketAddr {
        ([127,0,0,1],22222).into()
    }
}

fn main() {
    let mut evt_loop = Core::new().unwrap();

    let service = CarrierService {};

    let mut client = Client::new(service, evt_loop.handle());

    evt_loop.run(client).unwrap();
}
