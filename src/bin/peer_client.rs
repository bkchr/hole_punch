extern crate bytes;
extern crate env_logger;
extern crate futures;
extern crate h2;
extern crate hole_punch;
extern crate http;
extern crate hyper;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::dev_client::{Client, NewService, Service, ServiceControl, ServiceInformEvent};
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

fn do_http(con: hole_punch::PureConnection, handle: Handle) {
    println!("DO HTTP");

    let http: Http<hyper::Chunk> = Http::new();

    handle.spawn(
        http.serve_connection(con, HelloWorld)
            .map_err(|e| println!("\n\n\nERROR: {:?}\n\n\n", e))
            .map(|_| ()),
    );
}

fn do_http2(con: hole_punch::PureConnection, handle: Handle) {
    println!("DO HTTP2");

    let connection = h2::server::handshake(con)
        .and_then(|conn| {
            println!("H2 connection bound");

            conn.for_each(|(request, mut respond)| {
                println!("GOT request: {:?}", request);

                let response = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .body(())
                    .unwrap();

                let mut send = match respond.send_response(response, false) {
                    Ok(send) => send,
                    Err(e) => {
                        println!(" error respond; err={:?}", e);
                        return Ok(());
                    }
                };

                println!(">>>> sending data");
                if let Err(e) =
                    send.send_data(bytes::Bytes::from_static(b"hello world from http2"), true)
                {
                    println!("  -> err={:?}", e);
                }

                Ok(())
            })
        })
        .and_then(|_| {
            println!("~~~~~~~~~~~~~~~~~~~~~~~~~~~ H2 connection CLOSE !!!!!! ~~~~~~~~~~~");
            Ok(())
        })
        .then(|res| {
            if let Err(e) = res {
                println!("  -> err={:?}", e);
            }

            Ok(())
        });

    handle.spawn(connection);
}

fn main() {
    env_logger::init();
    let h2 = env::args().any(|a| a == "h2");
    // let server_addr = ([176, 9, 73, 99], 22222).into();
    let server_addr = ([127, 0, 0, 1], 22222).into();

    let mut evt_loop = Core::new().unwrap();

    loop {
        let new_service = NewCarrierService {};

        let mut client = Client::new(evt_loop.handle().clone(), new_service).expect("client");
        let addr: SocketAddr = server_addr;
        client.connect_to(&addr).expect("connect");

        let con = evt_loop
            .run(client.into_future().map_err(|_| ()))
            .unwrap()
            .0
            .unwrap();

        if h2 {
            do_http2(con, evt_loop.handle());
        } else {
            do_http(con, evt_loop.handle());
        }
    }
}
