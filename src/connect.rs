use errors::*;
use protocol;
use strategies::{self, ConnectTo};

use std::net::SocketAddr;
use std::mem;
use std::time::{Duration, Instant};
use std::collections::HashMap;

use tokio_core::reactor::{Handle, Timeout};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};

use serde::{Deserialize, Serialize};

enum ConnectState<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    None,
    Init,
    Connecting(<ConnectionHandler<P> as Stream>::Item, Timeout),
}

pub struct Connect<P>
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
    pub fn new(addr: SocketAddr, handle: &Handle) -> Connect<P> {
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
                        if let Ok(Ready(Some(protocol::Protocol::Acknowledge))) = con.1.poll() {
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

pub struct ConnectionHandler<P> {
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

pub struct DeviceToDeviceConnection<P> {
    strat: strategies::Strategy<P>,
    connections: HashMap<SocketAddr, strategies::Connection<P>>,
    timeout: Timeout,
}

impl<P> DeviceToDeviceConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn new(
        mut strat: strategies::Strategy<P>,
        addresses: &[SocketAddr],
        handle: &Handle,
    ) -> DeviceToDeviceConnection<P> {
        for addr in addresses {
            strat.connect(*addr);
        }

        DeviceToDeviceConnection {
            strat,
            connections: HashMap::new(),
            timeout: Timeout::new(Duration::from_millis(500), handle).unwrap(),
        }
    }
}

impl<P> Future for DeviceToDeviceConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (strategies::Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Ok(Ready(())) = self.timeout.poll() {
            self.timeout
                .reset(Instant::now() + Duration::from_millis(500));
            let _ = self.timeout.poll();

            for con in self.connections.values_mut() {
                con.start_send(protocol::Protocol::Hello)?;
                con.poll_complete()?;
            }
        }

        loop {
            match self.strat.poll() {
                Ok(Ready(Some(mut con))) => if let strategies::ConnectionType::Outgoing = con.2 {
                    con.0.start_send(protocol::Protocol::Hello)?;
                    con.0.poll_complete()?;
                    self.connections.insert(con.1, con.0);
                },
                _ => break,
            }
        }

        let mut address = None;
        {
            for (addr, con) in self.connections.iter_mut() {
                match con.poll() {
                    Ok(Ready(Some(val))) => if let protocol::Protocol::Hello = val {
                        address = Some(addr.clone());
                        break;
                    },
                    _ => {}
                }
            }
        }

        if let Some(address) = address {
            Ok(Ready(
                self.connections
                    .remove(&address)
                    .map(|v| (v, address))
                    .unwrap(),
            ))
        } else {
            Ok(NotReady)
        }
    }
}
