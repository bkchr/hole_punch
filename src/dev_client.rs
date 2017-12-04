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

enum ClientState<P> {
    None,
    EstablishConnection(ConnectionHandler<P>),
    Handshaking(ConnectionHandler<P>, Connect<P>),
    Connected(strategies::Connection<P>, bool, SocketAddr),
}

pub struct Client<S, P>
where
    S: Service<Message = P>,
{
    service: S,
    handle: Handle,
    state: ClientState<P>,
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
        mut new_con: bool,
        addr: SocketAddr,
    ) -> Poll<strategies::Connection<P>, Error> {
        loop {
            let answer = if new_con {
                new_con = false;
                self.service
                    .new_connection(addr)
                    .map(|v| protocol::Protocol::Embedded(v))
            } else {
                let msg = match con.poll()? {
                    Ready(Some(msg)) => msg,
                    Ready(None) => bail!("connect returned None!"),
                    NotReady => return Ok(NotReady),
                };

                match msg {
                    protocol::Protocol::Embedded(msg) => self.service
                        .on_message(&msg)?
                        .map(|v| protocol::Protocol::Embedded(v)),
                    _ => None,
                }
            };

            if let Some(msg) = answer {
                self.send_message(msg, con)?;
            }
        }
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
        loop {
            let (state, result) = match mem::replace(&mut self.state, ClientState::None) {
                ClientState::None => (
                    ClientState::EstablishConnection(ConnectionHandler::new(
                        self.service.connect_to(),
                        &self.handle,
                    )),
                    None,
                ),
                ClientState::EstablishConnection(mut handler) => {
                    if let Ok(Ready(Some(con))) = handler.poll() {
                        (ClientState::Handshaking(handler, Connect::new(con.0)), None)
                    } else {
                        (
                            ClientState::EstablishConnection(handler),
                            Some(Ok(NotReady)),
                        )
                    }
                }
                ClientState::Handshaking(mut handler, mut connect) => {
                    if let Ok(Ready(con)) = connect.poll() {
                        (ClientState::Connected(con, true, handler.get_addr()), None)
                    } else {
                        (
                            ClientState::Handshaking(handler, connect),
                            Some(Ok(NotReady)),
                        )
                    }
                }
                ClientState::Connected(mut con, new_con, addr) => {
                    let result = self.handle_connection(&mut con, new_con, addr);
                    (ClientState::Connected(con, false, addr), Some(result))
                }
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}

enum ConnectState<P> {
    None,
    Init(strategies::Connection<P>),
    Connecting(strategies::Connection<P>),
}

struct Connect<P> {
    state: ConnectState<P>,
}

impl<P> Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(con: strategies::Connection<P>) -> Connect<P> {
        Connect {
            state: ConnectState::Init(con),
        }
    }
}

impl<P> Future for Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = strategies::Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let (state, result) = match mem::replace(&mut self.state, ConnectState::None) {
                ConnectState::Init(mut con) => {
                    // TODO: do not use expect
                    con.start_send(protocol::Protocol::Register).expect("");
                    con.poll_complete().expect("");

                    (ConnectState::Connecting(con), None)
                }
                ConnectState::Connecting(mut con) => {
                    if let Ok(Ready(Some(protocol::Protocol::KeepAlive))) = con.poll() {
                        return Ok(Ready(con));
                    } else {
                        (ConnectState::Connecting(con), Some(Ok(NotReady)))
                    }
                }
                ConnectState::None => bail!("polled after connection established!"),
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}

struct ConnectionHandler<P> {
    // this should be ordered!
    strategies: Vec<strategies::Strategy<P>>,
    dest_addr: SocketAddr,
    active_strat: Option<strategies::Strategy<P>>,
}

impl<P> ConnectionHandler<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(dest_addr: SocketAddr, handle: &Handle) -> ConnectionHandler<P> {
        let strategies = strategies::connect(handle);

        ConnectionHandler {
            strategies,
            dest_addr,
            active_strat: None,
        }
    }

    fn get_addr(&self) -> SocketAddr {
        self.dest_addr
    }
}

impl<P> Stream for ConnectionHandler<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = <strategies::Strategy<P> as Stream>::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.active_strat.is_none() {
            self.active_strat = self.strategies.pop().map(|mut s| {
                s.connect(self.dest_addr);
                s
            });
        }

        if let Some(ref mut strat) = self.active_strat {
            match strat.poll() {
                Err(_) | Ok(Ready(None)) | Ok(NotReady) => return Ok(NotReady),
                Ok(Ready(Some(con))) => {
                    return if let strategies::ConnectionType::Outgoing = con.2 {
                        Ok(Ready(Some(con)))
                    } else {
                        Ok(NotReady)
                    }
                }
            };
        }

        bail!("No more strategies left!")
    }
}
