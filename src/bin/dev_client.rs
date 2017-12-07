#[macro_use]
extern crate error_chain;
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
use std::io::{self, Write};
use std::cell::RefCell;

use futures::{Future, Stream};

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
        Some(CarrierProtocol::RequestDevice {
            name: "nice".to_string(),
        })
    }

    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>> {
        match msg {
            &CarrierProtocol::Registered => {
                println!("REGISTERED");
                Ok(None)
            }
            &CarrierProtocol::DeviceNotFound => {
                println!("DEVICENOTFOUND");
                bail!("NOT FOUND");
            }
            _ => Ok(None),
        }
    }

    fn connect_to(&self) -> SocketAddr {
        ([176, 9, 73, 99], 22222).into()
    }
}

struct HttpConnector {
    con: RefCell<Option<hole_punch::PureConnection>>,
}

impl hyper::server::Service for HttpConnector {
    type Request = hyper::Uri;
    type Response = hole_punch::PureConnection;
    type Error = io::Error;
    type Future = Box<futures::Future<Item = Self::Response, Error = io::Error>>;

    fn call(&self, _: hyper::Uri) -> Self::Future {
        futures::future::ok(self.con.borrow_mut().take().unwrap()).boxed()
    }
}

fn main() {
    let mut evt_loop = Core::new().unwrap();

    let service = CarrierService {};

    let mut client = Client::new(service, evt_loop.handle());

    let con = evt_loop.run(client).unwrap();

    let client = hyper::Client::configure()
        .connector(HttpConnector {
            con: RefCell::new(Some(con)),
        })
        .build(&evt_loop.handle());

    let work = client
        .get("http://hello.carrier".parse().unwrap())
        .and_then(|res| {
            println!("Status: {}", res.status());
            println!("Headers:\n{}", res.headers());
            res.body().for_each(|chunk| {
                ::std::io::stdout()
                    .write_all(&chunk)
                    .map(|_| ())
                    .map_err(From::from)
            })
        });
    evt_loop.run(work).unwrap();
}
