extern crate futures;
extern crate hole_punch;
extern crate hyper;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::dev_client::{Client, Service};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use std::net::SocketAddr;

use hyper::{Request, Response};
use hyper::header::ContentLength;
use hyper::server::Http;

use futures::Future;

#[derive(Deserialize, Serialize)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
    RequestDevice { name: String },
    DeviceNotFound,
}

struct CarrierService {}

impl Service for CarrierService {
    type Message = CarrierProtocol;

    fn new_connection(&mut self, addr: SocketAddr) -> Option<Self::Message> {
        println!("Yeah, new connection, {:?}", addr);
        Some(CarrierProtocol::Register {
            name: "nice".to_string(),
        })
    }

    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>> {
        match msg {
            &CarrierProtocol::Registered => {
                println!("REGISTERED");
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn connect_to(&self) -> SocketAddr {
        ([176, 9, 73, 99], 22222).into()
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
        let service = CarrierService {};

        let client = Client::new(service, evt_loop.handle());

        let con = evt_loop.run(client).unwrap();

        let http: Http<hyper::Chunk> = Http::new();
        evt_loop.handle().spawn(
            http.serve_connection(con, HelloWorld)
                .map_err(|_| ())
                .map(|_| ()),
        );
    }
}
