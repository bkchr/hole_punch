extern crate env_logger;
extern crate futures;
extern crate hole_punch;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::context;
use hole_punch::errors::*;

use tokio_core::reactor::{Core, Handle};

use std::net::SocketAddr;
use std::thread;
use std::time::Duration;
use std::env;

use hyper::{Request, Response};
use hyper::header::ContentLength;
use hyper::server::Http;

use futures::{Future, Stream};

#[derive(Deserialize, Serialize, Clone)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
    RequestDevice { name: String },
    DeviceNotFound,
    AlreadyConnected,
}

struct CarrierConnection {
    stream: context::Stream<CarrierProtocol>,
}

struct CarrierService {
    control: ServiceControl<CarrierProtocol>,
}

impl Service for CarrierService {
    type Message = CarrierProtocol;

    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>> {
        match msg {
            &CarrierProtocol::Registered => {
                println!("REGISTERED");
                Ok(None)
            }
            &CarrierProtocol::RequestDevice { ref name } => {
                println!("REQUEST: {}", name);
                if name == "nice" {
                    Ok(Some(CarrierProtocol::AlreadyConnected))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn inform(&mut self, evt: ServiceInformEvent) {
        match evt {
            ServiceInformEvent::Connecting => println!("CONNECTING"),
        }
    }
}

struct NewCarrierService {}

impl NewService<CarrierProtocol> for NewCarrierService {
    type Service = CarrierService;

    fn new_service(
        &mut self,
        mut control: ServiceControl<CarrierProtocol>,
        addr: SocketAddr,
    ) -> Self::Service {
        println!("new connection to: {}", addr);

        control.send_message(CarrierProtocol::Register {
            name: "nice".to_owned(),
        });
        CarrierService { control }
    }
}

fn main() {
    env_logger::init();
    // let server_addr = ([176, 9, 73, 99], 22222).into();
    let server_addr = ([127, 0, 0, 1], 22222).into();

    let mut evt_loop = Core::new().unwrap();

    let new_service = NewCarrierService {};

    let mut client = Client::new(evt_loop.handle().clone(), new_service, false).expect("client");
    let addr: SocketAddr = server_addr;
    client.connect_to(&addr).expect("connect");

    let handle = evt_loop.handle();
    evt_loop
        .run(client.for_each(move |con| {
            let handle = handle.clone();
            if h2 {
                do_http2(con, handle);
            } else {
                do_http(con, handle);
            }
            Ok(())
        }))
        .unwrap();
}
