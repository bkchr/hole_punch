#[macro_use]
extern crate futures;
extern crate hole_punch;
extern crate runners;
#[macro_use]
extern crate structopt;
extern crate tokio_core;

use runners::protocol::Protocol;

use hole_punch::{
    Config, Context, Error, FileFormat, ResolvePeer, ResolvePeerResult, Stream, StreamHandle,
};

use tokio_core::reactor::Core;

use std::{
    collections::HashMap, sync::{Arc, Mutex},
};

use futures::Async::Ready;
use futures::{Future, Poll, Stream as FStream};

use structopt::StructOpt;

struct ServerContextInner {
    devices: HashMap<String, StreamHandle<Protocol, ServerContext>>,
}

#[derive(Clone)]
struct ServerContext {
    inner: Arc<Mutex<ServerContextInner>>,
}

impl ServerContext {
    fn new() -> ServerContext {
        ServerContext {
            inner: Arc::new(Mutex::new(ServerContextInner {
                devices: HashMap::new(),
            })),
        }
    }

    fn remove_peer(&self, peer: &str) {
        self.inner.lock().unwrap().devices.remove(peer);
    }

    fn add_peer(&self, peer: &str, handle: StreamHandle<Protocol, ServerContext>) {
        self.inner
            .lock()
            .unwrap()
            .devices
            .insert(peer.into(), handle);
    }
}

impl ResolvePeer<Protocol> for ServerContext {
    type Identifier = String;
    fn resolve_peer(&self, peer: &Self::Identifier) -> ResolvePeerResult<Protocol, Self> {
        match self.inner.lock().unwrap().devices.get(peer) {
            Some(handle) => ResolvePeerResult::FoundLocally(handle.clone()),
            None => ResolvePeerResult::NotFound,
        }
    }
}

unsafe impl Send for ServerContext {}
unsafe impl Sync for ServerContext {}

struct Connection {
    stream: Stream<Protocol, ServerContext>,
    name: Option<String>,
    server_context: ServerContext,
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
                        self.server_context.remove_peer(name);
                    }
                    return Ok(Ready(()));
                }
            };

            match msg {
                Protocol::Register(name) => {
                    self.name = Some(name.clone());
                    println!("New peer: {}", name);
                    self.server_context
                        .add_peer(&name, self.stream.get_stream_handle());
                    self.stream.upgrade_to_authenticated();
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

    let server_context = ServerContext::new();

    let cert = include_bytes!("../../certs/cert.pem");
    let key = include_bytes!("../../certs/key.pem");

    let mut config = Config::new();
    config.set_cert_chain(vec![cert.to_vec()], FileFormat::PEM);
    config.set_key(key.to_vec(), FileFormat::PEM);
    config.set_quic_listen_port(options.listen_port);

    let server = Context::new(
        "server".into(),
        evt_loop.handle(),
        config,
        server_context.clone(),
    ).unwrap();
    println!("Server up and running");

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
