extern crate env_logger;
#[macro_use]
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

use futures::{Future, Stream, Sink, Poll};
use futures::Async::{NotReady, Ready };
use futures::stream::{StreamFuture, FuturesUnordered };

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

impl Future for CarrierConnection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
       loop {
           let msg = try_ready!(self.stream.poll()) {
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
               _ => {},
           };
       } 
    }
}

struct Carrier {
    cons: FuturesUnordered<StreamFuture<context::Connection<CarrierProtocol>>>,
    handle: Handle,
    name: String,
}

impl Future for Rc<RefCell<Carrier>> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let stream = match try_ready!(self.borrow_mut().cons.poll()) {
                Some((Some(stream), con)) =>  {
                    self.cons.push(con.into_future());
                    stream
                },
                // yeah, we should not do that, but we will manually poll, when we add new cons
                None => return Ok(NotReady),
                Some((None, con)) => {
                    println!("connection closed");
                    continue;
                }
            };

            handle.spawn(CarrierConnection {stream, name: self.borrow().name.clone()}.map_err(|e| println!("connection error {:?}", e)));
        }
    }
}

fn main() {
    env_logger::init();
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // let server_addr = ([176, 9, 73, 99], 22222).into();
    let server_addr: SocketAddr = ([127, 0, 0, 1], 22222).into();

    let mut evt_loop = Core::new().unwrap();

    let config = config::Config {
        udp_listen_address: ([0, 0, 0, 0], 0).into(),
        cert_file: PathBuf::from(format!("{}/src/bin/cert.pem", manifest_dir)),
        key_file: PathBuf::from(format!("{}/src/bin/key.pem", manifest_dir)),
    };

    let context = context::Context::new(evt_loop.handle(), config).unwrap();

    let server_con = context.create_connection_to_server(server_addr);

    let handle = evt_loop.handle();
    evt_loop
        .run(client.for_each(move |con| {
            let handle = handle.clone();
            
            Ok(())
        }))
        .unwrap();
}
