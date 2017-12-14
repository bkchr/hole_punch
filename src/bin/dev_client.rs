#[macro_use]
extern crate error_chain;
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
use std::io::{self, Write};
use std::cell::RefCell;

use futures::{Future, Stream};

#[derive(Deserialize, Serialize)]
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
            &CarrierProtocol::DeviceNotFound => {
                println!("DEVICENOTFOUND");
                bail!("NOT FOUND");
            },
            &CarrierProtocol::AlreadyConnected => {
                println!("ALREADY");
                self.control.use_as_result();
                Ok(None)
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

        control.send_message(CarrierProtocol::RequestDevice {
            name: "nice".to_owned(),
        });
        CarrierService { control }
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
    //([176, 9, 73, 99], 22222).into()
    let mut evt_loop = Core::new().unwrap();

    let new_service = NewCarrierService {};

    let mut client = Client::new(evt_loop.handle(), new_service).expect("client");
    let addr: SocketAddr = ([127, 0, 0, 1], 22222).into();
    client.connect_to(&addr).expect("connect");

    let con = evt_loop
        .run(client.into_future().map_err(|_| ()))
        .unwrap()
        .0
        .unwrap();

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
