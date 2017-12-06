use errors::*;
use protocol;
use strategies;
use connect::Connect;

use std::net::SocketAddr;
use std::mem;
use std::time::{Duration, Instant};

use tokio_core::reactor::{Handle, Timeout};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};

use serde::{Deserialize, Serialize};

use pnet_datalink::interfaces;

use itertools::Itertools;

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
        Timeout,
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
    ) -> Poll<strategies::Connection<P>, Error> {
        if let Ok(Ready(())) = timeout.poll() {
            if self.received_keepalive {
                self.received_keepalive = false;
                self.send_message(protocol::Protocol::KeepAlive, con)?;
                timeout.reset(Instant::now() + Duration::new(30, 0));
                let _ = timeout.poll();
                println!("NO TIMEOUT");
            } else {
                println!("TIMEOUT");
                bail!("TIMEOUT");
            }
        }

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
                protocol::Protocol::Connect { public, private, .. } => {
                    println!("CONNECT: {:?}, {:?}", public, private);
                    None
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
                    let result = self.handle_connection(&mut con, &mut timeout);
                    (
                        ClientState::Connected(strat, con, addr, timeout),
                        Some(result),
                    )
                }
            };

            self.state = state;

            if let Some(result) = result {
                return result;
            }
        }
    }
}
