use errors::*;
use udp;
use protocol::Protocol;
use strategies::{self, Connection, Strategy};

use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use tokio_core::reactor::{Core, Handle};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};

use serde::{Deserialize, Serialize};

pub trait Service {
    type Message;
    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>>;
    fn close(&self);
}

pub type ServiceId = u64;

pub trait NewService {
    type Service;
    fn new_service(&self, id: ServiceId) -> Self::Service;
}

struct ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    connection: Connection<P>,
    service: T,
    receiver: UnboundedReceiver<Protocol<P>>,
    id: ServiceId,
    state: Arc<Mutex<State<P>>>,
}

impl<T, P> ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn send_message(&self, msg: Protocol<P>) -> Result<()> {
        self.connection
            .start_send(msg)
            .chain_err(|| "error sending message")?;
        self.connection
            .poll_complete()
            .chain_err(|| "error sending message")
            .map(|_| ())
    }

    fn poll_impl(&mut self) -> Poll<(), Error> {
        let msg = match self.connection.poll()? {
            Ready(Some(msg)) => msg,
            Ready(None) => return Ok(Ready(())),
            NotReady => return Ok(NotReady),
        };

        let answer = match msg {
            Protocol::Embedded(v) => self.service.on_message(&v)?.map(|v| Protocol::Embedded(v)),
            _ => None,
        };

        if let Some(answer) = answer {
            self.send_message(answer)?;
        }

        Ok(NotReady)
    }
}

impl<T, P> Future for ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
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
    sockets: Vec<Strategy<P>>,
    new_service: N,
    state: Arc<Mutex<State<P>>>,
    handle: Handle,
}

impl<N, P> Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service<Message = P> + 'static,
    P: Serialize + for<'de> Deserialize<'de> + 'static,
{
    pub fn new(new_service: N, handle: Handle) -> Server<N, P> {
        let state = Arc::new(Mutex::new(State::new()));
        let sockets = strategies::accept(&handle);

        Server {
            sockets,
            new_service,
            state,
            handle,
        }
    }

    pub fn run(self, evt_loop: &Core) -> Result<()> {
        evt_loop.run(self)
    }
}

impl<N, P> Future for Server<N, P>
where
    N: NewService,
    <N as NewService>::Service: Service<Message = P> + 'static,
    P: Serialize + for<'de> Deserialize<'de> + 'static,
{
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        for socket in self.sockets.iter() {
            match socket.poll()? {
                Ready(Some(con)) => {
                    self.state.new_service(|id| {
                        let service = self.new_service.new_service(id);

                        let (sender, receiver) = unbounded();

                        let handler = ServiceHandler {
                            connection: con.0,
                            service,
                            receiver,
                            id,
                            state: self.state.clone(),
                        };

                        self.handle.spawn(handler.map_err(|_| ()));

                        sender
                    });

                    continue;
                }
                Ready(None) => {
                    bail!("strategy returned None!");
                }
                _ => {}
            }
        }

        Ok(NotReady)
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
}

impl<P> ServerState<P> for Arc<Mutex<State<P>>> {
    fn new_service<F>(&self, create_service: F)
    where
        F: FnOnce(ServiceId) -> UnboundedSender<Protocol<P>>,
    {
        let state = self.lock().unwrap();

        let service_id = state
            .unused_ids
            .pop()
            .unwrap_or_else(|| state.services.len() as u64);

        let sender = create_service(service_id);

        state.services.insert(service_id, sender);
    }

    fn free_service(&self, id: ServiceId) {
        let state = self.lock().unwrap();

        if state.services.remove(&id).is_some() {
            state.unused_ids.push(id);
        }
    }
}
