use errors::*;
use protocol::Protocol;
use strategies::{self, Connection, Strategy, Connect, WaitForConnect};

use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::collections::{HashMap, LinkedList};

use tokio_core::reactor::{Handle, self};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

struct Timeout(reactor::Timeout, Duration);

impl Timeout {
    fn new(dur: Duration,handle: &Handle) -> Timeout {
        Timeout(reactor::Timeout::new(dur, handle).expect("no timeout!!"), dur)
    }

    fn reset(self) -> Self {
        self.0.reset(Instant::now() + self.1);
        self
    }
}

impl Future for Timeout {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());

        // if we come to this point, the timer finished, aka timeout!
        bail!("Timeout")
    }
}

#[derive(StateMachineFuture)]
enum ConnectStateMachine<P: 'static + Serialize + for<'de> Deserialize<'de>> {
    #[state_machine_future(start, transitions(WaitingForConnect))]
    Init {
        connect: Connect,
        addr: SocketAddr,
        handle: Handle,
    },
    #[state_machine_future(transitions(WaitingForRegisterResponse))]
    WaitingForConnect {
        connect: WaitForConnect<P>,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(Connected))]
    WaitingForRegisterResponse {
        connection: Connection<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)] Connected(Connection<P>),
    #[state_machine_future(error)] ErrorState(Error),
}

impl<P> PollConnectStateMachine<P>
    for ConnectStateMachine<P> where P: 'static + Serialize + for<'de> Deserialize<'de>{
        fn poll_init<'a>(
            init: &'a mut RentToOwn<'a, Init>,
        ) -> Poll<AfterInit<P>, Error> {
            let init = init.take();

            let connect = init.connect.connect(init.addr);

            Ok(Ready(
                WaitingForConnect {
                    connect,
                    timeout: Timeout::new(init.handle, Duration::from_millis(500)),
                }.into(),
            ))
        }
    fn poll_waiting_for_connect<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForConnect<P>>,
    ) -> Poll<AfterWaitingForConnect<P>, Error> {
try_ready!(wait.timeout.poll().chain_err(|| "wait for connect"));
        let connection = try_ready!(wait.connect.poll());

        let wait = wait.take();

        Ok(Ready(
            WaitingForRegisterReponse {
                connection,
                timeout: wait.timeout.reset(),
            }.into(),
        ))
    }

    fn poll_waiting_for_register_response<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForRegisterResponse<P>>,
    ) -> Poll<AfterWaitingForRegisterResponse<P>, Error> {
        try_ready!(init.timeout.poll().chain_err(|| "wait for register response"));

        let resp = try_ready!(wait.connection.poll());

        match resp {
            Some(Protocol::Acknowledge) => {
                let wait = wait.take();

                Ok(Ready(Connected(wait.connection).into()))
            }
            None => bail!("connection returned None"),
            _ => Ok(NotReady),
        }
    }
}

struct ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    strategies: Vec<Connect>,
    connect: ConnectStateMachineFuture<P>,
    addr: SocketAddr,
    handle: Handle,
}

impl<'connect, P> ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn new(
        mut strategies: Vec<Connect>,
        handle: Handle,
        addr: SocketAddr,
    ) -> ConnectWithStrategies<P> {
        let strategy = strategies
            .pop()
            .expect("At least one strategy should be given!");

        ConnectWithStrategies {
            strategies,
            addr,
            connect: ConnectStateMachine::start(strategy, addr, handle.clone()),
            handle,
        }
    }
}

impl<P> Future for ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.connect.poll() {
            Ok(Ready(con)) => Ok(Ready(con)),
            Ok(NotReady) => Ok(NotReady),
            Err(e) => {
                println!("error: {:?}", e);

                match self.strategies.pop() {
                    Some(mut strat) => {
                        self.connect = ConnectStateMachine::start(strat, self.addr, self.handle.clone());
                        let _ = self.connect.poll()?;
                        Ok(NotReady)
                    }
                    None => bail!("No strategies left for connecting to: {}", self.addr),
                }
            }
        }
    }
}

pub struct Connector {
    handle: Handle,
    strategies: Vec<Connect>,
}

impl Connector {
    pub fn new(handle: Handle, strategies: Vec<Connect>) -> Connector {
        Connector {
            handle,
            strategies,
        }
    }

    pub fn connect<P>(&self, addr: SocketAddr) -> ConnectWithStrategies<P>
        P: Serialize + for<'de> Deserialize<'de>
    {
        ConnectWithStrategies::new(self.strategies.clone(), self.handle.clone(), addr)
    }
}

pub struct DeviceToDeviceConnection<P> {
    strat: Strategy<P>,
    connections: HashMap<SocketAddr, Connection<P>>,
    timeout: Timeout,
}

impl<P> DeviceToDeviceConnection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn new(
        mut strat: Strategy<P>,
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
    type Item = (Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.timeout.poll().is_err() {
            self.timeout = self.timeout.reset();
            let _ = self.timeout.poll();

            for con in self.connections.values_mut() {
                con.start_send(Protocol::Hello)?;
                con.poll_complete()?;
            }
        }

        loop {
            match self.strat.poll() {
                Ok(Ready(Some(mut con))) => if let strategies::ConnectionType::Outgoing = con.2 {
                    con.0.start_send(Protocol::Hello)?;
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
                    Ok(Ready(Some(val))) => if let Protocol::Hello = val {
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
