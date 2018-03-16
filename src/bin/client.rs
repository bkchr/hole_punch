extern crate either;
extern crate env_logger;
#[macro_use]
extern crate futures;
extern crate hole_punch;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;

use hole_punch::{Config, ConnectionId, Context, Error, Stream};

use tokio_core::reactor::{Core, Handle};

use std::net::SocketAddr;
use std::io::{self, Write};
use std::cell::RefCell;
use std::path::PathBuf;

use futures::{Future, Poll, Sink, Stream as FStream};
use futures::Async::{NotReady, Ready};
use futures::stream::{FuturesUnordered, StreamFuture};

#[derive(Deserialize, Serialize, Clone)]
enum Protocol {
    SendMessage(String),
    ReceiveMessage(String),
    }

struct CarrierConnection {
    stream: Stream<Protocol>,
    context: Context<Protocol>,
    name: String,
    request_name: String,
    handle: Handle,
}

impl CarrierConnection {
    fn register(&mut self) {
        self.stream.start_send(Protocol::Register {
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
            match self.context.poll() {
                Ok(NotReady) => break,
                Err(e) => {
                    println!("{:?}", e);
                    break;
                }
                _ => {}
            }
        }
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => {
                    println!("stream closed!");
                    return Ok(Ready(()));
                }
            };

            match msg {
                Protocol::Registered => {
                    println!("REGISTERED");
                    println!("Requesting connection to: {}", self.request_name);

                    let connection_id = self.context.generate_connection_id();

                    self.handle.spawn(
                        self.context
                            .create_connection_to_peer(
                                connection_id,
                                &mut self.stream,
                                Protocol::RequestDevice {
                                    name: self.request_name.clone(),
                                    connection_id,
                                },
                            )
                            .unwrap()
                            .and_then(|_| {
                                println!("CREATED CONNECTION!!!");
                                Ok(())
                            })
                            .map_err(|e| println!("{:?}", e)),
                    );
                }
                Protocol::DeviceNotFound => {
                    panic!("device not found");
                }
                _ => {}
            };
        }
    }
}

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let server_addr: SocketAddr = ([127, 0, 0, 1], 22222).into();

    let mut evt_loop = Core::new().unwrap();

    let config = Config::new(
        ([0, 0, 0, 0], 0).into(),
        PathBuf::from(format!("{}/src/bin/cert.pem", manifest_dir)),
        PathBuf::from(format!("{}/src/bin/key.pem", manifest_dir)),
    );

    let mut context = Context::new(evt_loop.handle(), config).unwrap();
    let server_con = context.create_connection_to_server(&server_addr);

    let handle = evt_loop.handle();

    evt_loop
        .run(server_con.and_then(|stream| {
            let mut con = CarrierConnection {
                stream,
                handle,
                context,
                name: "dev".to_owned(),
                request_name: "nice".to_owned(),
            };
            con.register();

            con
        }))
        .unwrap();
}