extern crate futures;
extern crate hole_punch;
extern crate hyper;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::dev_client::{Client, NewService, Service, ServiceControl, ServiceInformEvent};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

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
            },
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

struct HelloWorld;

const PHRASE: &'static str = "Hello, from the universe!";

impl hyper::server::Service for HelloWorld {
    // boilerplate hooking up hyper's server types
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    // The future representing the eventual Response your call will
    // resolve to. This can change to whatever Future you need.
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, _req: Request) -> Self::Future {
        // We're currently ignoring the Request
        // And returning an 'ok' Future, which means it's ready
        // immediately, and build a Response with the 'PHRASE' body.
        Box::new(futures::future::ok(
            Response::new()
                .with_header(ContentLength(PHRASE.len() as u64))
                .with_body(PHRASE),
        ))
    }
}

fn main() {
    let mut evt_loop = Core::new().unwrap();

    loop {
        let new_service = NewCarrierService {};

        let mut client = Client::new(evt_loop.handle().clone(), new_service).expect("client");
        let addr: SocketAddr = ([127, 0, 0, 1], 22222).into();
        client.connect_to(&addr).expect("connect");

        let con = evt_loop
            .run(client.into_future().map_err(|_| ()))
            .unwrap()
            .0
            .unwrap();

        println!("AFTER SLEEP");
        let http: Http<hyper::Chunk> = Http::new();
        evt_loop.handle().spawn(
            http.serve_connection(con, HelloWorld)
                .map_err(|e| println!("\n\n\nERROR: {:?}\n\n\n", e))
                .map(|_| ()),
        );
    }
}
