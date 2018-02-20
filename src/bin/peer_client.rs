extern crate env_logger;
#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::{config, context};
use hole_punch::errors::*;

use tokio_core::reactor::{Core, Handle};

use std::net::SocketAddr;
use std::cell::RefCell;
use std::rc::Rc;
use std::path::PathBuf;

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::stream::{FuturesUnordered, StreamFuture};

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
    name: String,
}

impl CarrierConnection {
    fn register(&mut self) {
        self.stream.start_send(CarrierProtocol::Register {
            name: self.name.clone(),
        });
        self.stream.poll_complete();
    }
}

impl Future for CarrierConnection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => {
                    println!("stream closed!");
                    return Ok(Ready(()));
                }
            };

            match msg {
                CarrierProtocol::Registered => {
                    println!("REGISTERED");
                }
                CarrierProtocol::RequestDevice { name } => {
                    println!("REQUEST: {}", name);
                    if name == self.name {
                        self.stream.start_send(CarrierProtocol::AlreadyConnected);
                        self.stream.poll_complete();
                    }
                }
                _ => {}
            };
        }
    }
}

fn main() {
    env_logger::init();
    let name: String = "nice".into();
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // let server_addr = ([176, 9, 73, 99], 22222).into();
    let server_addr: SocketAddr = ([127, 0, 0, 1], 22222).into();

    let mut evt_loop = Core::new().unwrap();

    let config = config::Config {
        udp_listen_address: ([0, 0, 0, 0], 0).into(),
        cert_file: PathBuf::from(format!("{}/src/bin/cert.pem", manifest_dir)),
        // cert_file: PathBuf::from("cert.pem"),
        // key_file: PathBuf::from("key.pem"),
        key_file: PathBuf::from(format!("{}/src/bin/key.pem", manifest_dir)),
    };

    let mut context = context::Context::new(evt_loop.handle(), config).unwrap();
    let server_con = context.create_connection_to_server(&server_addr);

    let handle = evt_loop.handle();

    let name2 = name.clone();
    evt_loop.handle().spawn(
        context
            .for_each(move |stream| {
                let name = name2.clone();
                println!("NEW CARRIER");

                handle.spawn(
                    CarrierConnection {
                        stream,
                        name: name.clone(),
                    }.map_err(|e| println!("{:?}", e)),
                );

                Ok(())
            })
            .map_err(|e| println!("{:?}", e)),
    );

    evt_loop
        .run(server_con.and_then(|stream| {
            let mut stream = CarrierConnection {
                stream,
                name: name.clone(),
            };

            stream.register();

            stream
        }))
        .unwrap();
}
