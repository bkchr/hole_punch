use errors::*;
use protocol::Protocol;
use strategies::{self, Connection, NewSessionWait, Strategy};

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::net::SocketAddr;

use tokio_core::reactor::{Core, Handle};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

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

enum InterServiceProtocol<P> {
    SendMessage(Protocol<P>),
    RelayConnection(ServiceId),
    NewSessionCreated(NewSessionWait<P>),
}

#[derive(StateMachineFuture)]
enum ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForRelayMode, Finished))]
    ServiceMode {
        connection: Connection<P>,
        service: T,
        receiver: UnboundedReceiver<InterServiceProtocol<P>>,
        id: ServiceId,
        state: Arc<Mutex<State<P>>>,
        address: SocketAddr,
        handle: Handle,
    },
    #[state_machine_future(transitions(Finished))]
    WaitForRelayMode {
        connection: Connection<P>,
        new_session: NewSessionWait<P>,
        handle: Handle,
    },
    #[state_machine_future(ready)] Finished(()),
    //TODO: We are not removing the service if an error occurs....
    #[state_machine_future(error)] ErrorState(Error),
}

impl<T, P> PollServiceHandler<T, P> for ServiceHandler<T, P>
where
    T: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_service_mode<'a>(
        mode: &'a mut RentToOwn<'a, ServiceMode<T, P>>,
    ) -> Poll<AfterServiceMode<P>, Error> {
        loop {
            let msg = match mode.connection.poll()? {
                Ready(Some(msg)) => msg,
                Ready(None) => {
                    mode.service.close();
                    mode.state.free_service(mode.id);
                    return Ok(Ready(Finished(()).into()));
                }
                NotReady => break,
            };

            let answer = match msg {
                Protocol::Embedded(v) => {
                    mode.service.on_message(&v)?.map(|v| Protocol::Embedded(v))
                }
                Protocol::Register => {
                    eprintln!("REGISTER: {}", mode.id);
                    Some(Protocol::Acknowledge)
                }
                Protocol::KeepAlive => Some(Protocol::KeepAlive),
                Protocol::PrivateAdressInformation(id, mut addresses) => {
                    addresses.push(mode.address);
                    let connect = Protocol::Connect(addresses, 0, mode.id);
                    mode.state
                        .send_message(id, InterServiceProtocol::SendMessage(connect))?;

                    None
                }
                Protocol::RelayConnection(id) => {
                    eprintln!("RELAYCONNECTION: {} --- {}", mode.id, id);
                    mode.state
                        .send_message(id, InterServiceProtocol::RelayConnection(mode.id))?;
                    None
                }
                _ => None,
            };

            if let Some(answer) = answer {
                mode.connection.send_and_poll(answer);
            }

            if let Some(msg) = mode.service.request_control_message() {
                match msg {
                    ServiceControlMessage::CreateConnectionTo(id) => {
                        mode.state.send_message(
                            id,
                            InterServiceProtocol::SendMessage(
                                Protocol::RequestPrivateAdressInformation(mode.id),
                            ),
                        )?;
                        mode.connection
                            .send_and_poll(Protocol::RequestPrivateAdressInformation(id));
                    }
                }
            }
        }

        loop {
            match mode.receiver.poll() {
                Ok(Ready(Some(msg))) => match msg {
                    InterServiceProtocol::SendMessage(msg) => {
                        mode.connection.send_and_poll(msg);
                    }
                    InterServiceProtocol::RelayConnection(other) => {
                        let new_session = mode.connection.new_session();
                        mode.state.send_message(
                            other,
                            InterServiceProtocol::NewSessionCreated(new_session),
                        )?;
                    }
                    InterServiceProtocol::NewSessionCreated(new_session) => {
                        let mut mode = mode.take();

                        mode.service.close();
                        mode.state.free_service(mode.id);
                        mode.connection.send_and_poll(Protocol::RelayModeActivated);

                        let connection = mode.connection;
                        let handle = mode.handle;

                        return Ok(Ready(
                            WaitForRelayMode {
                                connection,
                                new_session,
                                handle,
                            }.into(),
                        ));
                    }
                },
                _ => break,
            }
        }

        Ok(NotReady)
    }

    fn poll_wait_for_relay_mode<'a>(
        mode: &'a mut RentToOwn<'a, WaitForRelayMode<P>>,
    ) -> Poll<AfterWaitForRelayMode, Error> {
        let con = try_ready!(mode.new_session.poll());

        let mode = mode.take();

        let (con_sink, con_stream) = mode.connection.into_pure().split();
        let (other_sink, other_stream) = con.into_pure().split();

        mode.handle.spawn(
            con_stream
                .map(|v| v.try_mut().unwrap())
                .forward(other_sink)
                .map(|_| ())
                .map_err(|e| eprintln!("connection relaying error: {:?}", e)),
        );
        mode.handle.spawn(
            other_stream
                .map(|v| v.try_mut().unwrap())
                .forward(con_sink)
                .map(|_| ())
                .map_err(|e| eprintln!("connection relaying error: {:?}", e)),
        );

        Ok(Ready(Finished(()).into()))
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

                        let handler = ServiceHandler::start(
                            con,
                            service,
                            receiver,
                            id,
                            self.state.clone(),
                            remote_addr,
                            self.handle.clone(),
                        );

                        self.handle
                            .spawn(handler.map_err(|e| eprintln!("Error: {:?}", e)));

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
    services: HashMap<ServiceId, UnboundedSender<InterServiceProtocol<P>>>,
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
        F: FnOnce(ServiceId) -> UnboundedSender<InterServiceProtocol<P>>;

    fn free_service(&self, id: ServiceId);

    fn send_message(&self, id: ServiceId, msg: InterServiceProtocol<P>) -> Result<()>;
}

impl<P> ServerState<P> for Arc<Mutex<State<P>>> {
    fn new_service<F>(&self, create_service: F)
    where
        F: FnOnce(ServiceId) -> UnboundedSender<InterServiceProtocol<P>>,
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

    fn send_message(&self, id: ServiceId, msg: InterServiceProtocol<P>) -> Result<()> {
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
