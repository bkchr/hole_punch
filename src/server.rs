use errors::*;
use protocol::Protocol;
use strategies::{self, Connection, Strategy};

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::net::SocketAddr;

use tokio_core::reactor::{Core, Handle};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};

use serde::{Deserialize, Serialize};

pub enum ServiceControlMessage {
    CreateConnectionTo(ServiceId),
}

pub trait Service {
    type Message;
    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>>;
    fn close(&self);
    fn request_control_message(&mut self) -> Option<ServiceControlMessage>;
}

pub type ServiceId = u64;

pub trait NewService {
    type Service;
    fn new_service(&self, id: ServiceId) -> Self::Service;
}

struct ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    connection: Connection<P>,
    service: T,
    receiver: UnboundedReceiver<Protocol<P>>,
    id: ServiceId,
    state: Arc<Mutex<State<P>>>,
    address: SocketAddr,
}

impl<T, P> ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn send_message(&mut self, msg: Protocol<P>) -> Result<()> {
        self.connection
            .start_send(msg)
            .chain_err(|| "error sending message")?;
        self.connection
            .poll_complete()
            .chain_err(|| "error sending message")
            .map(|_| ())
    }

    fn poll_impl(&mut self) -> Poll<(), Error> {
        loop {
            let msg = match self.connection.poll()? {
                Ready(Some(msg)) => msg,
                Ready(None) => return Ok(Ready(())),
                NotReady => break,
            };

            let answer = match msg {
                Protocol::Embedded(v) => {
                    self.service.on_message(&v)?.map(|v| Protocol::Embedded(v))
                }
                Protocol::Register => {
                    println!("REGISTER");
                    Some(Protocol::Acknowledge)
                }
                Protocol::KeepAlive => Some(Protocol::KeepAlive),
                Protocol::PrivateAdressInformation(id, mut addresses) => {
                    addresses.push(self.address);
                    let connect = Protocol::Connect(addresses, 0);
                    self.state.send_message(id, connect)?;

                    None
                }
                _ => None,
            };

            if let Some(answer) = answer {
                self.send_message(answer)?;
            }

            if let Some(msg) = self.service.request_control_message() {
                match msg {
                    ServiceControlMessage::CreateConnectionTo(id) => {
                        self.state
                            .send_message(id, Protocol::RequestPrivateAdressInformation(self.id))?;
                        self.send_message(Protocol::RequestPrivateAdressInformation(id))?;
                    }
                }
            }
        }

        loop {
            match self.receiver.poll() {
                Ok(Ready(Some(msg))) => self.send_message(msg)?,
                _ => break,
            }
        }

        Ok(NotReady)
    }
}

impl<T, P> Future for ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.poll_impl() {
            r @ Ok(NotReady) => r,
            r @ _ => {
                self.service.close();
                self.state.free_service(self.id);
                r
            }
        }
    }
}

pub struct Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service,
{
    sockets: FuturesUnordered<StreamFuture<Strategy<P>>>,
    new_service: N,
    state: Arc<Mutex<State<P>>>,
    handle: Handle,
}

impl<N, P> Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service<Message = P> + 'static,
    P: Serialize + for<'de> Deserialize<'de> + 'static + Clone,
{
    pub fn new(new_service: N, handle: Handle) -> Result<Server<N, P>> {
        let state = Arc::new(Mutex::new(State::new()));
        let sockets = strategies::accept(&handle).chain_err(|| "failed to create sockets")?;

        Ok(Server {
            sockets: futures_unordered(sockets.into_iter().map(|v| v.into_future())),
            new_service,
            state,
            handle,
        })
    }

    pub fn run(self, evt_loop: &mut Core) -> Result<()> {
        evt_loop.run(self)
    }
}

impl<N, P> Future for Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service<Message = P> + 'static,
    P: Serialize + for<'de> Deserialize<'de> + 'static + Clone,
{
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let (con, strat) = match try_ready!(self.sockets.poll().map_err(|(e, _)| e)) {
                Some(v) => v,
                None => bail!("no sockets left, looks like an error!"),
            };

            match con {
                Some(con) => {
                    self.state.new_service(|id| {
                        let service = self.new_service.new_service(id);

                        let (sender, receiver) = unbounded();

                        let remote_addr = con.remote_addr();

                        let handler = ServiceHandler {
                            connection: con,
                            address: remote_addr,
                            service,
                            receiver,
                            id,
                            state: self.state.clone(),
                        };

                        self.handle
                            .spawn(handler.map_err(|e| println!("Error: {:?}", e)));

                        sender
                    });
                }
                None => {
                    bail!("strategy returned None!");
                }
            }

            self.sockets.push(strat.into_future());
        }
    }
}

struct State<P> {
    services: HashMap<ServiceId, UnboundedSender<Protocol<P>>>,
    unused_ids: Vec<ServiceId>,
}

impl<P> State<P> {
    fn new() -> State<P> {
        State {
            services: HashMap::new(),
            unused_ids: Vec::new(),
        }
    }
}

trait ServerState<P> {
    fn new_service<F>(&self, create_service: F)
    where
        F: FnOnce(ServiceId) -> UnboundedSender<Protocol<P>>;

    fn free_service(&self, id: ServiceId);

    fn send_message(&self, id: ServiceId, msg: Protocol<P>) -> Result<()>;
}

impl<P> ServerState<P> for Arc<Mutex<State<P>>> {
    fn new_service<F>(&self, create_service: F)
    where
        F: FnOnce(ServiceId) -> UnboundedSender<Protocol<P>>,
    {
        let mut state = self.lock().unwrap();

        let service_id = state
            .unused_ids
            .pop()
            .unwrap_or_else(|| state.services.len() as u64);

        let sender = create_service(service_id);

        state.services.insert(service_id, sender);
    }

    fn free_service(&self, id: ServiceId) {
        let mut state = self.lock().unwrap();

        if state.services.remove(&id).is_some() {
            state.unused_ids.push(id);
        }
    }

    fn send_message(&self, id: ServiceId, msg: Protocol<P>) -> Result<()> {
        let state = self.lock().unwrap();

        if let Some(sender) = state.services.get(&id) {
            sender
                .unbounded_send(msg)
                .map_err(|_| "error sending message".into())
        } else {
            bail!("could not find requested instance for sending message")
        }
    }
}
