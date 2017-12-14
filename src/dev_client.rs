use errors::*;
use protocol::Protocol;
use strategies::{self, Connection, PureConnection, Strategy};
use connect::{Connector, DeviceToDeviceConnection};

use std::net::{SocketAddr, ToSocketAddrs};
use std::mem;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_core::reactor::{Handle, Timeout};

use futures::{Future, IntoFuture, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::sync::oneshot;
use futures::stream::{FuturesUnordered, StreamFuture};

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use itertools::Itertools;

use state_machine_future::RentToOwn;

pub trait NewService<P>
where
    Self::Service: Service<Message = P>,
{
    type Service;
    fn new_service(&mut self, control: ServiceControl<P>, addr: SocketAddr) -> Self::Service;
}

pub trait Service {
    type Message;
    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>>;
    fn inform(&mut self, event: ServiceInformEvent);
}

pub enum ServiceInformEvent {
    Connecting,
}

pub enum ServiceControlEvent<P> {
    CloseConnection,
    SendMessage(P),
    UseAsResult,
}

pub struct ServiceControl<P> {
    sender: UnboundedSender<ServiceControlEvent<P>>,
}

impl<P> ServiceControl<P> {
    fn new() -> (ServiceControl<P>, UnboundedReceiver<ServiceControlEvent<P>>) {
        let (sender, receiver) = unbounded();

        (ServiceControl { sender }, receiver)
    }

    pub fn close(&mut self) {
        let _ = self.sender
            .unbounded_send(ServiceControlEvent::CloseConnection);
    }

    pub fn send_message(&mut self, msg: P) {
        let _ = self.sender
            .unbounded_send(ServiceControlEvent::SendMessage(msg));
    }

    pub fn use_as_result(&mut self) {
        let _ = self.sender.unbounded_send(ServiceControlEvent::UseAsResult);
    }
}

pub struct ClientInner<P, N>
where
    N: NewService<P>,
{
    strategies: Vec<Strategy<P>>,
    connector: Connector,
    new_service: N,
}

type ClientInnerSync<P, N> = Arc<Mutex<ClientInner<P, N>>>;

pub struct Client<P, N>
where
    N: NewService<P>,
{
    inner: ClientInnerSync<P, N>,
    handle: Handle,
    result_recvs: FuturesUnordered<StreamFuture<UnboundedReceiver<Result<PureConnection>>>>,
}

impl<P, N> Client<P, N>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
    N: NewService<P> + 'static,
{
    pub fn new(handle: Handle, new_service: N) -> Result<Client<P, N>> {
        let (strategies, connects) =
            strategies::connect(&handle).chain_err(|| "error creating strategy sockets")?;

        let connector = Connector::new(handle.clone(), connects);

        Ok(Client {
            inner: Arc::new(Mutex::new(ClientInner {
                strategies,
                connector,
                new_service,
            })),
            handle,
            result_recvs: FuturesUnordered::new(),
        })
    }

    pub fn connect_to<A: ToSocketAddrs>(&mut self, addrs: A) -> Result<()> {
        let addr_and_sender = addrs
            .to_socket_addrs()
            .chain_err(|| "error getting socket addresses")?
            .map(|s| {
                let (sender, receiver) = unbounded();
                self.result_recvs.push(receiver.into_future());
                (s.clone(), sender)
            })
            .collect::<Vec<_>>();

        let handle = self.handle.clone();
        let inner = self.inner.clone();
        self.handle.spawn_fn(move || {
            for (addr, sender) in addr_and_sender {
                let wait = inner.lock().unwrap().connector.connect(addr);
                handle.spawn(
                    ServiceHandler::start(
                        inner.clone(),
                        wait.map(move |(con, port)| (con, addr, port)),
                        handle.clone(),
                        sender,
                    ).map_err(|e| println!("{:?}", e)),
                );
            }

            Ok(())
        });

        Ok(())
    }
}

impl<P, N> Stream for Client<P, N>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
    N: NewService<P> + 'static,
{
    type Item = PureConnection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let result = match self.result_recvs.poll() {
            Ok(Ready(Some(val))) => val,
            _ => return Ok(NotReady),
        };

        match result {
            (Some(con), receiver) => {
                self.result_recvs.push(receiver.into_future());

                Ok(Ready(Some(con.unwrap())))
            }
            _ => Ok(NotReady),
        }
    }
}

#[derive(StateMachineFuture)]
enum ServiceHandler<
    P: 'static + Serialize + for<'de> Deserialize<'de>,
    N: NewService<P> + 'static,
    F: Future<Item = (Connection<P>, SocketAddr, u16), Error = Error>,
> {
    #[state_machine_future(start, transitions(HandleMessages))]
    WaitForConnection {
        client: ClientInnerSync<P, N>,
        wait: F,
        handle: Handle,
        result_sender: UnboundedSender<Result<PureConnection>>,
    },
    #[state_machine_future(transitions(Finished))]
    HandleMessages {
        client: ClientInnerSync<P, N>,
        result_sender: UnboundedSender<Result<PureConnection>>,
        handle: Handle,
        connection: Connection<P>,
        remote_addr: SocketAddr,
        local_port: u16,
        service: <N as NewService<P>>::Service,
        service_control_receiver: UnboundedReceiver<ServiceControlEvent<P>>,
    },
    #[state_machine_future(ready)] Finished(()),
    #[state_machine_future(error)] ErrorState(Error),
}

impl<P, N, F> PollServiceHandler<P, N, F> for ServiceHandler<P, N, F>
where
    P: Serialize + for<'de> Deserialize<'de>,
    N: NewService<P> + 'static,
    F: Future<Item = (Connection<P>, SocketAddr, u16), Error = Error>,
{
    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection<P, N, F>>,
    ) -> Poll<AfterWaitForConnection<P, N>, Error> {
        let (connection, remote_addr, local_port) = try_ready!(wait.wait.poll());

        let wait = wait.take();
        let (servicec, service_control_receiver) = ServiceControl::new();

        let service = wait.client
            .lock()
            .unwrap()
            .new_service
            .new_service(servicec, remote_addr);

        Ok(Ready(
            HandleMessages {
                client: wait.client,
                connection,
                remote_addr,
                local_port,
                handle: wait.handle,
                result_sender: wait.result_sender,
                service,
                service_control_receiver,
            }.into(),
        ))
    }

    fn poll_handle_messages<'a>(
        handler: &'a mut RentToOwn<'a, HandleMessages<P, N>>,
    ) -> Poll<AfterHandleMessages, Error> {
        loop {
            let message = match handler.connection.poll() {
                Ok(Ready(message)) => message,
                Ok(NotReady) => break,
                Err(e) => return Err(e),
            };

            let message = match message {
                Some(message) => message,
                None => bail!("connection({}) closed", handler.remote_addr),
            };

            let answer = match message {
                Protocol::Embedded(msg) => handler
                    .service
                    .on_message(&msg)?
                    .map(|v| Protocol::Embedded(v)),
                Protocol::KeepAlive => None,
                Protocol::RequestPrivateAdressInformation(id) => {
                    let addresses = interfaces()
                        .iter()
                        .map(|v| v.ips.clone())
                        .concat()
                        .iter()
                        .map(|v| v.ip())
                        .filter(|ip| !ip.is_loopback())
                        .map(|ip| (ip, handler.local_port).into())
                        .collect_vec();

                    Some(Protocol::PrivateAdressInformation(id, addresses))
                }
                Protocol::Connect(addresses, _) => {
                    println!("CONNECT: {:?}", addresses);
                    handler.service.inform(ServiceInformEvent::Connecting);
                    let connector = handler.client.lock().unwrap().connector.clone();
                    let wait =
                        DeviceToDeviceConnection::new(connector, &addresses, &handler.handle);

                    handler.handle.spawn(
                        ServiceHandler::start(
                            handler.client.clone(),
                            wait,
                            handler.handle.clone(),
                            handler.result_sender.clone(),
                        ).map_err(|e| println!("{:?}", e)),
                    );
                    None
                },
                Protocol::Hello => { println!("HELLO"); Some(Protocol::Hello) },
                _ => None,
            };

            if let Some(answer) = answer {
                handler.connection.send_and_poll(answer);
            }
        };

        loop {
            let msg = match handler.service_control_receiver.poll() { Ok(Ready(Some(msg))) => msg, _ => return Ok(NotReady) };

            match msg {
                ServiceControlEvent::UseAsResult => {
                    let handler = handler.take();
                    println!("USE");
                    handler.result_sender.unbounded_send(Ok(handler.connection.into_pure()));
                    return Ok(Ready(Finished(()).into()));
                },
                ServiceControlEvent::SendMessage(msg) => {
                    handler.connection.send_and_poll(Protocol::Embedded(msg));
                }
                _ => {}
            };
        }
    }
}
