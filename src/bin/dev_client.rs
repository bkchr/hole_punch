extern crate env_logger;
#[macro_use]
extern crate error_chain;
#[macro_use]
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

use tokio_core::reactor::Core;

use std::net::SocketAddr;
use std::io::{self, Write};
use std::cell::RefCell;
use std::env;

use futures::{Future, IntoFuture, Poll, Stream};

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
            &CarrierProtocol::Register { ref name } => {
                println!("REGISTER: {}", name);
                Ok(None)
            }
            &CarrierProtocol::DeviceNotFound => {
                println!("DEVICENOTFOUND");
                bail!("NOT FOUND");
            }
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
    type Future = futures::future::FutureResult<Self::Response, io::Error>;

    fn call(&self, _: hyper::Uri) -> Self::Future {
        futures::future::ok(self.con.borrow_mut().take().unwrap())
    }
}

fn do_http(con: hole_punch::PureConnection, mut evt_loop: Core) {
    println!("DOING HTTP");
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

struct Process {
    body: h2::RecvStream,
    trailers: bool,
}

impl Future for Process {
    type Item = ();
    type Error = h2::Error;

    fn poll(&mut self) -> Poll<(), h2::Error> {
        loop {
            if self.trailers {
                let trailers = try_ready!(self.body.poll_trailers());

                println!("GOT TRAILERS: {:?}", trailers);

                return Ok(().into());
            } else {
                match try_ready!(self.body.poll()) {
                    Some(chunk) => {
                        println!("GOT CHUNK = {:?}", String::from_utf8(chunk.to_vec()));
                    }
                    None => {
                        self.trailers = true;
                    }
                }
            }
        }
    }
}

fn do_http2(con: hole_punch::PureConnection, mut evt_loop: Core) {
    println!("DOING HTTP2");

    let handle = evt_loop.handle();

    let work = h2::client::handshake(con).into_future().then(|res| {
        let (mut client, h2) = res.unwrap();

        println!("sending request");

        let request = http::Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let mut trailers = http::HeaderMap::new();
        trailers.insert("zomg", "hello".parse().unwrap());

        let (response, mut stream) = client.send_request(request, false).unwrap();

        // send trailers
        stream.send_trailers(trailers).unwrap();

        // Spawn a task to run the conn...
        handle.spawn(h2.map_err(|e| println!("GOT ERR1={:?}", e)));

        println!("{:?}", response);

        response
            .and_then(|response| {
                println!("GOT RESPONSE: {:?}", response);

                // Get the body
                let (_, body) = response.into_parts();

                Process {
                    body,
                    trailers: false,
                }
            })
            .map_err(|e| {
                println!("GOT ERR={:?}", e);
            })
    });

    evt_loop.run(work).unwrap();
}

fn main() {
    env_logger::init();
    let h2 = env::args().any(|a| a == "h2");
    let server_addr = ([176, 9, 73, 99], 22222).into();
    let mut evt_loop = Core::new().unwrap();

    let new_service = NewCarrierService {};

    let mut client = Client::new(evt_loop.handle(), new_service).expect("client");
    let addr: SocketAddr = server_addr;
    client.connect_to(&addr).expect("connect");

    let con = evt_loop
        .run(client.into_future().map_err(|_| ()))
        .unwrap()
        .0
        .unwrap();

    if h2 {
        do_http2(con, evt_loop);
    } else {
        do_http(con, evt_loop);
    }
}
