use errors::*;
use udp;
use protocol;
use strategies::{self, ConnectTo};

use std::net::SocketAddr;
use std::mem;

use tokio_core::reactor::Handle;
use tokio_timer;

use futures::{self, Future, Poll, Sink, StartSend, Stream};
use futures::Async::{NotReady, Ready};

use pnet_datalink::interfaces;

use itertools::Itertools;

use serde::{Deserialize, Serialize};

pub trait Service {
    type Message;
    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>>;
    fn new_connection(&mut self, addr: SocketAddr) -> Option<Self::Message>;
    fn connect_to(&self) -> SocketAddr;
}

pub struct Client<S, P>
where
    S: Service<Message = P>,
{
    service: S,
    handle: Handle,
    connect: Connect<P>,
    last_connected_address: Option<SocketAddr>,
}

impl<S, P> Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn new(service: S, handle: Handle) -> Client<S, P> {
        let connect = Connect::new(service.connect_to(), &handle);

        Client {
            service,
            handle,
            connect,
            last_connected_address: None,
        }
    }

    fn send_message(&mut self, msg: protocol::Protocol<P>) -> Result<()> {
        self.connect
            .start_send(msg)
            .chain_err(|| "error sending message")?;
        self.connect
            .poll_complete()
            .chain_err(|| "error sending message")
            .map(|_| ())
    }
}

impl<S, P> Future for Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = strategies::Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.connect.get_addr() != self.service.connect_to() {
            self.connect = Connect::new(self.service.connect_to(), &self.handle);
        }

        let msg = match self.connect.poll()? {
            Ready(Some(msg)) => msg,
            Ready(None) => bail!("connect returned None!"),
            NotReady => return Ok(NotReady),
        };

        let answer = if self.connect.is_connected()
            && Some(self.connect.get_addr()) != self.last_connected_address
        {
            self.last_connected_address = Some(self.connect.get_addr());
            self.service
                .new_connection(self.connect.get_addr())
                .map(|v| protocol::Protocol::Embedded(v))
        } else {
            match msg {
                protocol::Protocol::Embedded(msg) => self.service
                    .on_message(&msg)?
                    .map(|v| protocol::Protocol::Embedded(v)),
                _ => None,
            }
        };

        if let Some(msg) = answer {
            self.send_message(msg)?;
        }

        Ok(NotReady)
    }
}

enum ConnectState<P> {
    None,
    Initiating(strategies::Strategy<P>),
    Connecting((strategies::Connection<P>, strategies::Strategy<P>)),
    Connected((strategies::Connection<P>, strategies::Strategy<P>)),
}

struct Connect<P> {
    strategies: Vec<strategies::Strategy<P>>,
    state: ConnectState<P>,
    addr: SocketAddr,
    address_infos: protocol::AddressInformation,
}

impl<P> Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(addr: SocketAddr, handle: &Handle) -> Connect<P> {
        let mut strategies = strategies::connect(handle);
        let addresses = interfaces()
            .iter()
            .map(|v| v.ips.clone())
            .concat()
            .iter()
            .map(|v| v.ip())
            .filter(|ip| !ip.is_loopback())
            .collect_vec();

        Connect {
            strategies: strategies,
            addr,
            state: ConnectState::None,
            address_infos: protocol::AddressInformation { port: 0, addresses },
        }
    }

    fn get_addr(&self) -> SocketAddr {
        self.addr
    }

    fn is_connected(&self) -> bool {
        match self.state {
            ConnectState::Connected(_) => true,
            _ => false,
        }
    }
}

impl<P> Stream for Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = protocol::Protocol<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let (state, result) = match mem::replace(&mut self.state, ConnectState::None) {
                ConnectState::None => {
                    let mut initial = self.strategies.pop().unwrap();
                    initial.connect(self.addr);
                    println!("Initial");
                    (ConnectState::Initiating(initial), None)
                }
                ConnectState::Initiating(mut server) => match server.poll() {
                    Ok(Ready(Some((mut con, _, cty)))) => {
                        if let strategies::ConnectionType::Outgoing = cty {
                            println!("Out");
                            self.address_infos.port = server.local_addr().unwrap().port();
                            // TODO: do not use expect
                            con.start_send(protocol::Protocol::Register {
                                private: self.address_infos.clone(),
                            }).expect("");
                            con.poll_complete().expect("");

                            (ConnectState::Connecting((con, server)), None)
                        } else {
                            (ConnectState::Initiating(server), Some(Ok(NotReady)))
                        }
                    }
                    Err(_) | Ok(Ready(None)) => (ConnectState::None, None),
                    Ok(NotReady) => (ConnectState::Initiating(server), Some(Ok(NotReady))),
                },
                ConnectState::Connecting((mut con, mut server)) => {
                    println!("CONNECTING!");
                    server.poll()?;
                    match con.poll() {
                        Ok(Ready(Some(msg))) => if let protocol::Protocol::KeepAlive = msg {
                            (ConnectState::Connected((con, server)), Some(Ok(Ready(Some(msg)))))
                        } else {
                            (ConnectState::Connecting((con, server)), Some(Ok(NotReady)))
                        },
                        Ok(NotReady) => {
                            (ConnectState::Connecting((con, server)), Some(Ok(NotReady)))
                        }
                        Err(_) | Ok(Ready(None)) => (ConnectState::None, None),
                    }
                }
                ConnectState::Connected((mut con, server)) => {
                    let result = con.poll();
                    (ConnectState::Connected((con, server)), Some(result))
                }
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}

impl<P> Sink for Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type SinkItem = protocol::Protocol<P>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.state {
            ConnectState::Connected((ref mut con, _)) => con.start_send(item),
            _ => bail!("Not connected!"),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self.state {
            ConnectState::Connected((ref mut con, _)) => con.poll_complete(),
            _ => bail!("Not connected!"),
        }
    }
}
