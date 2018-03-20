#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate tokio_core;

use runners::protocol::Protocol;

use hole_punch::{Config, Context, Error, Stream, StreamHandle};

use tokio_core::reactor::Core;

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;

use futures::{Future, Poll, Stream as FStream};
use futures::Async::Ready;

use structopt::StructOpt;

struct ServerContext {
    devices: HashMap<String, StreamHandle<Protocol>>,
}

struct Connection {
    stream: Stream<Protocol>,
    name: Option<String>,
    server_context: Rc<RefCell<ServerContext>>,
}

impl Future for Connection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let msg = match try_ready!(self.stream.poll()) {
                Some(msg) => msg,
                None => {
                    println!("Connection closed: {:?}", self.name);
                    if let Some(ref name) = self.name {
                        self.server_context.borrow_mut().devices.remove(name);
                    }
                    return Ok(Ready(()));
                }
            };

            match msg {
                Protocol::Register(name) => {
                    self.name = Some(name.clone());
                    println!("New peer: {}", name);
                    self.server_context
                        .borrow_mut()
                        .devices
                        .insert(name, self.stream.get_stream_handle());
                    self.stream.upgrade_to_authenticated();
                }
                Protocol::RequestPeer(name, connection_id) => {
                    println!("Requesting peer: {}", name);

                    if let Some(mut handle) =
                        self.server_context.borrow_mut().devices.get_mut(&name)
                    {
                        self.stream
                            .create_connection_to(connection_id, &mut handle)?;
                    } else {
                        self.stream.send_and_poll(Protocol::PeerNotFound)?;
                    }
                }
                _ => {}
            };
        }
    }
}

#[derive(StructOpt)]
struct Options {
    #[structopt(long = "listen_port")]
    listen_port: u16,
}

fn main() {
    let options = Options::from_args();
    let mut evt_loop = Core::new().unwrap();

    let server_context = Rc::new(RefCell::new(ServerContext {
        devices: HashMap::new(),
    }));

    let mut config = Config::new();
    config.set_cert_chain_filename("./cert.pem");
    config.set_key_filename("./key.pem");
    config.set_quic_listen_port(options.listen_port);

    let server = Context::new(evt_loop.handle(), config).unwrap();

    let handle = evt_loop.handle();
    let server = server.for_each(|stream| {
        handle.spawn(
            Connection {
                stream: stream,
                name: None,
                server_context: server_context.clone(),
            }.map_err(|e| panic!(e)),
        );
        Ok(())
    });

    evt_loop.run(server).unwrap();
}
