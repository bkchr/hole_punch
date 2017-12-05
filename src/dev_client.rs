use errors::*;
use udp;
use protocol;
use strategies::{self, ConnectTo};

use std::net::SocketAddr;
use std::mem;
use std::time::Duration;

use tokio_core::reactor::{Handle, Timeout};
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
    ),
}

pub struct Client<S, P>
where
    S: Service<Message = P>,
    P: Serialize + for<'de> Deserialize<'de>,
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
    ) -> Poll<strategies::Connection<P>, Error> {
        loop {
            let msg = match con.poll()? {
                Ready(Some(msg)) => msg,
                Ready(None) => bail!("connect returned None!"),
                NotReady => return Ok(NotReady),
            };

            let answer = match msg {
                protocol::Protocol::Embedded(msg) => self.service
                    .on_message(&msg)?
                    .map(|v| protocol::Protocol::Embedded(v)),
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
    ) -> Result<()> {
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
    type Item = strategies::Connection<P>;
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
                        self.handle_new_connection(&mut con, addr)?;
                        (ClientState::Connected(strat, con, addr), None)
                    }
                    _ => (ClientState::Connecting(con), Some(Ok(NotReady))),
                },
                ClientState::Connected(strat, mut con, addr) => {
                    let result = self.handle_connection(&mut con);
                    (ClientState::Connected(strat, con, addr), Some(result))
                }
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}

enum ConnectState<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    None,
    Init,
    Connecting(<ConnectionHandler<P> as Stream>::Item, Timeout),
}

struct Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    state: ConnectState<P>,
    connection_handler: ConnectionHandler<P>,
    handle: Handle,
}

impl<P> Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn new(addr: SocketAddr, handle: &Handle) -> Connect<P> {
        let chandler = ConnectionHandler::new(addr, handle);

        Connect {
            state: ConnectState::Init,
            connection_handler: chandler,
            handle: handle.clone(),
        }
    }
}

impl<P> Future for Connect<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = <ConnectionHandler<P> as Stream>::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let (state, result) = match mem::replace(&mut self.state, ConnectState::None) {
                ConnectState::Init => {
                    match self.connection_handler.poll()? {
                        Ready(Some(mut con)) => {
                            // TODO: do not use expect
                            con.1.start_send(protocol::Protocol::Register).expect("");
                            con.1.poll_complete().expect("");

                            let timeout = Timeout::new(Duration::new(1, 0), &self.handle)
                                .chain_err(|| "error creating timeout")?;

                            (ConnectState::Connecting(con, timeout), None)
                        }
                        _ => (ConnectState::Init, Some(Ok(NotReady))),
                    }
                }
                ConnectState::Connecting(mut con, mut timeout) => {
                    if let Ok(Ready(())) = timeout.poll() {
                        (ConnectState::Init, None)
                    } else {
                        if let Ok(Ready(Some(protocol::Protocol::KeepAlive))) = con.1.poll() {
                            return Ok(Ready(con));
                        } else {
                            (ConnectState::Connecting(con, timeout), Some(Ok(NotReady)))
                        }
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
}

impl<P> Stream for ConnectionHandler<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (
        strategies::Strategy<P>,
        strategies::Connection<P>,
        SocketAddr,
    );
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.active_strat.is_none() {
            self.active_strat = self.strategies.pop().map(|mut s| {
                s.connect(self.dest_addr);
                s
            });
        }

        if let Some(mut strat) = self.active_strat.take() {
            let (strat, result) = match strat.poll() {
                Err(_) | Ok(Ready(None)) => (None, Ok(NotReady)),
                Ok(NotReady) => (Some(strat), Ok(NotReady)),
                Ok(Ready(Some(con))) => if let strategies::ConnectionType::Outgoing = con.2 {
                    (None, Ok(Ready(Some((strat, con.0, con.1)))))
                } else {
                    (Some(strat), Ok(NotReady))
                },
            };

            self.active_strat = strat;
            return result;
        }

        bail!("No more strategies left!")
    }
}
