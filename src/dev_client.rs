use errors::*;
use protocol::{AddressInformation, Protocol};
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
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::sync::oneshot;

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use itertools::Itertools;

use state_machine_future::RentToOwn;

pub trait NewService {
    type Service;
    fn new_service(addr: SocketAddr) -> Self::Service;
}

pub trait Service {
    type Message;
    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>>;
}
/*
enum ClientProtocol {

}

pub struct ClientControl {
    sender: UnboundedSender<ClientProtocol>,
}

impl ClientControl {
    fn new(sender: UnboundedSender<ClientProtocol>) -> ClientControl {
        ClientControl { sender }
    }
}

enum ClientState<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    None,
    Connecting(Connect<P>),
    Connected(
        strategies::Strategy<P>,
        strategies::Connection<P>,
        SocketAddr,
        Timeout,
    ),
    DeviceToDevice(DeviceToDeviceConnection<P>),
}

pub struct Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    service: S,
    handle: Handle,
    state: ClientState<P>,
    received_keepalive: bool,
    strat_port: u16,
}

impl<S, P> Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn new(service: S, handle: Handle) -> Client<S, P> {
        Client {
            service,
            handle,
            state: ClientState::None,
            received_keepalive: true,
            strat_port: 0,
        }
    }

    fn send_message(
        &mut self,
        msg: protocol::Protocol<P>,
        con: &mut strategies::Connection<P>,
    ) -> Result<()> {
        con.start_send(msg).chain_err(|| "error sending message")?;
        con.poll_complete()
            .chain_err(|| "error sending message")
            .map(|_| ())
    }

    fn handle_connection(
        &mut self,
        con: &mut strategies::Connection<P>,
        timeout: &mut Timeout,
    ) -> Result<
        Either<
            Poll<strategies::PureConnection, Error>,
            (protocol::AddressInformation, protocol::AddressInformation),
        >,
    > {
        if let Ok(Ready(())) = timeout.poll() {
            if self.received_keepalive {
                self.received_keepalive = false;
                self.send_message(protocol::Protocol::KeepAlive, con)?;
                timeout.reset(Instant::now() + Duration::new(30, 0));
                let _ = timeout.poll();
            } else {
                bail!("TIMEOUT");
            }
        }

        loop {
            let msg = match con.poll()? {
                Ready(Some(msg)) => msg,
                Ready(None) => bail!("connect returned None!"),
                NotReady => return Ok(Left(Ok(NotReady))),
            };

            let answer = match msg {
                protocol::Protocol::Embedded(msg) => self.service
                    .on_message(&msg)?
                    .map(|v| protocol::Protocol::Embedded(v)),
                protocol::Protocol::KeepAlive => {
                    println!("KEEPALIVE");
                    self.received_keepalive = true;
                    None
                }
                protocol::Protocol::RequestPrivateAdressInformation(id) => {
                    let addresses = interfaces()
                        .iter()
                        .map(|v| v.ips.clone())
                        .concat()
                        .iter()
                        .map(|v| v.ip())
                        .filter(|ip| !ip.is_loopback())
                        .collect_vec();

                    Some(protocol::Protocol::PrivateAdressInformation(
                        id,
                        protocol::AddressInformation {
                            port: self.strat_port,
                            addresses,
                        },
                    ))
                }
                protocol::Protocol::Connect {
                    public, private, ..
                } => {
                    println!("CONNECT: {:?}, {:?}", public, private);
                    return Ok(Right((public, private)));
                }
                _ => None,
            };

            if let Some(msg) = answer {
                self.send_message(msg, con)?;
            }
        }
    }

    fn handle_new_connection(
        &mut self,
        con: &mut strategies::Connection<P>,
        addr: SocketAddr,
        strat_port: u16,
    ) -> Result<()> {
        self.strat_port = strat_port;
        if let Some(answer) = self.service.new_connection(addr) {
            self.send_message(protocol::Protocol::Embedded(answer), con)
        } else {
            Ok(())
        }
    }
}

impl<S, P> Future for Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = strategies::PureConnection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let (state, result) = match mem::replace(&mut self.state, ClientState::None) {
                ClientState::None => (
                    ClientState::Connecting(Connect::new(self.service.connect_to(), &self.handle)),
                    None,
                ),
                ClientState::Connecting(mut con) => match con.poll()? {
                    Ready((strat, mut con, addr)) => {
                        self.handle_new_connection(
                            &mut con,
                            addr,
                            strat.local_addr().unwrap().port(),
                        )?;
                        (
                            ClientState::Connected(
                                strat,
                                con,
                                addr,
                                Timeout::new(Duration::new(30, 0), &self.handle)?,
                            ),
                            None,
                        )
                    }
                    _ => (ClientState::Connecting(con), Some(Ok(NotReady))),
                },
                ClientState::Connected(strat, mut con, addr, mut timeout) => {
                    let result = self.handle_connection(&mut con, &mut timeout)?;

                    match result {
                        Left(result) => (
                            ClientState::Connected(strat, con, addr, timeout),
                            Some(result),
                        ),
                        Right((public, private)) => {
                            let mut addresses = public
                                .addresses
                                .iter()
                                .map(|a| SocketAddr::new(*a, public.port))
                                .collect::<Vec<_>>();
                            addresses.extend(
                                private
                                    .addresses
                                    .iter()
                                    .map(|a| SocketAddr::new(*a, private.port)),
                            );

                            (
                                ClientState::DeviceToDevice(DeviceToDeviceConnection::new(
                                    strat,
                                    addresses.as_slice(),
                                    &self.handle,
                                )),
                                None,
                            )
                        }
                    }
                }
                ClientState::DeviceToDevice(mut con) => match con.poll()? {
                    Ready((con, addr)) => {
                        println!("YEAH, connected to: {}", addr);
                        return Ok(Ready(con.into_pure()));
                    }
                    _ => (ClientState::DeviceToDevice(con), Some(Ok(NotReady))),
                },
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}
*/

pub struct Client<P> {
    handle: Handle,
    strategies: Vec<Strategy<P>>,
    connector: Connector,
    result_recv: oneshot::Receiver<Result<PureConnection>>,
    result_sender: oneshot::Sender<Result<PureConnection>>,
}

impl<P> Client<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn new(handle: Handle) -> Result<Client<P>> {
        let (strategies, connects) =
            strategies::connect(&handle).chain_err(|| "error creating strategy sockets")?;

        let connector = Connector::new(handle.clone(), connects);
        let (result_sender, result_recv) = oneshot::channel();

        Ok(Client {
            handle,
            strategies,
            connector,
            result_sender,
            result_recv,
        })
    }

    fn connect_to<A: ToSocketAddrs>(&self, addrs: A) -> Result<()> {
        let addrs = addrs
            .to_socket_addrs()
            .chain_err(|| "error getting socket addresses")?
            .map(|s| s.clone())
            .collect::<Vec<_>>();

        let handle = self.handle.clone();
        let connector = self.connector.clone();
        self.handle.spawn_fn(move || {
            for addr in addrs {
                handle.spawn(connector.connect::<P>(addr).map(|_| ()).map_err(|_| ()));
            }

            Ok(())
        });

        Ok(())
    }
}

#[derive(StateMachineFuture)]
enum ServiceHandler<P: 'static + Serialize + for<'de> Deserialize<'de>, S: Service<Message = P>> {
    #[state_machine_future(start, transitions(Finished, SendClientResult))]
    HandleMessages {
        result_sender: oneshot::Sender<Result<PureConnection>>,
        connection: Connection<P>,
        remote_addr: SocketAddr,
        local_port: u16,
        service: S,
    },
    #[state_machine_future(transitions(Finished))]
    SendClientResult {
        result_sender: oneshot::Sender<Result<PureConnection>>,
        connection: Connection<P>,
    },
    #[state_machine_future(ready)] Finished(()),
    #[state_machine_future(error)] ErrorState(Error),
}

impl<P, S> PollServiceHandler<P, S> for ServiceHandler<P, S>
where
    P: Serialize + for<'de> Deserialize<'de>,
    S: Service<Message = P>,
{
    fn poll_handle_messages<'a>(
        handler: &'a mut RentToOwn<'a, HandleMessages<P, S>>,
    ) -> Poll<AfterHandleMessages<P>, Error> {
        loop {
            let message = try_ready!(handler.connection.poll());

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
                        .collect_vec();

                    Some(Protocol::PrivateAdressInformation(
                        id,
                        AddressInformation {
                            port: handler.local_port,
                            addresses,
                        },
                    ))
                }
                Protocol::Connect {
                    public, private, ..
                } => {
                    println!("CONNECT: {:?}, {:?}", public, private);
                    None
                }
                _ => None,
            };

            handler.connection.send_and_poll(answer.unwrap());
        }
    }

    fn poll_send_client_result<'a>(
        send: &'a mut RentToOwn<'a, SendClientResult<P>>,
    ) -> Poll<AfterSendClientResult, Error> {
        let send = send.take();
        let _ = send.result_sender.send(Ok(send.connection.into_pure()));

        Ok(Ready(Finished(()).into()))
    }
}
