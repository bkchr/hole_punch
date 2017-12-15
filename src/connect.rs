use errors::*;
use protocol::Protocol;
use strategies::{Connect, Connection, WaitForConnect};

use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::mem::discriminant;

use tokio_core::reactor::{self, Handle};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::stream::{futures_unordered, FuturesUnordered};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

pub struct Timeout(reactor::Timeout, Duration);

impl Timeout {
    fn new(dur: Duration, handle: &Handle) -> Timeout {
        Timeout(
            reactor::Timeout::new(dur, handle).expect("no timeout!!"),
            dur,
        )
    }

    fn reset(&mut self) {
        self.0.reset(Instant::now() + self.1);
    }

    fn new_reset(mut self) -> Self {
        self.reset();
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
        wait_for_message: WaitForMessage<P>,
        port: u16,
        timeout: Timeout,
    },
    #[state_machine_future(ready)] Connected((Connection<P>, u16)),
    #[state_machine_future(error)] ErrorState(Error),
}

impl<P> PollConnectStateMachine<P> for ConnectStateMachine<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn poll_init<'a>(init: &'a mut RentToOwn<'a, Init>) -> Poll<AfterInit<P>, Error> {
        let mut init = init.take();

        let connect = init.connect
            .connect(init.addr)
            .chain_err(|| "error connecting")?;

        Ok(Ready(
            WaitingForConnect {
                connect,
                timeout: Timeout::new(Duration::from_millis(1000), &init.handle),
            }.into(),
        ))
    }

    fn poll_waiting_for_connect<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForConnect<P>>,
    ) -> Poll<AfterWaitingForConnect<P>, Error> {
        if wait.timeout.poll().is_err() { bail!("wait for connect timeout") };
        let mut connection = try_ready!(wait.connect.poll());

        let wait = wait.take();
        connection.0.send_and_poll(Protocol::Register);

        Ok(Ready(
            WaitingForRegisterResponse {
                wait_for_message: WaitForMessage::new(connection.0, Protocol::Acknowledge),
                port: connection.1,
                timeout: wait.timeout.new_reset(),
            }.into(),
        ))
    }

    fn poll_waiting_for_register_response<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForRegisterResponse<P>>,
    ) -> Poll<AfterWaitingForRegisterResponse<P>, Error> {
        if wait.timeout.poll().is_err() { bail!("wait for register response") };

        let connection = try_ready!(wait.wait_for_message.poll());

        Ok(Ready(Connected((connection, wait.port)).into()))
    }
}

pub struct ConnectWithStrategies<P>
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
    type Item = (Connection<P>, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.connect.poll() {
            Ok(Ready(con)) => Ok(Ready(con)),
            Ok(NotReady) => Ok(NotReady),
            Err(e) => {
                println!("error: {:?}", e);

                match self.strategies.pop() {
                    Some(mut strat) => {
                        self.connect =
                            ConnectStateMachine::start(strat, self.addr, self.handle.clone());
                        let _ = self.connect.poll()?;
                        Ok(NotReady)
                    }
                    None => bail!("No strategies left for connecting to: {}", self.addr),
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Connector {
    handle: Handle,
    strategies: Vec<Connect>,
}

impl Connector {
    pub fn new(handle: Handle, strategies: Vec<Connect>) -> Connector {
        Connector { handle, strategies }
    }

    pub fn connect<P>(&self, addr: SocketAddr) -> ConnectWithStrategies<P>
    where
        P: Serialize + for<'de> Deserialize<'de>,
    {
        ConnectWithStrategies::new(self.strategies.clone(), self.handle.clone(), addr)
    }

    fn get_connect(&self) -> Connect {
        //HACK!!
        self.strategies.first().unwrap().clone()
    }
}

pub struct WaitForMessage<P>(Option<Connection<P>>, Protocol<P>);

impl<P> WaitForMessage<P> {
    pub fn new(con: Connection<P>, msg: Protocol<P>) -> WaitForMessage<P> {
        WaitForMessage(Some(con), msg)
    }
}

impl<P> Future for WaitForMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let message = match try_ready!(
            self.0
                .as_mut()
                .expect("can not be polled when message was already received")
                .poll()
        ) {
            Some(message) => message,
            None => bail!("connection closed while waiting for Message"),
        };

        if discriminant(&self.1) == discriminant(&message) {
            Ok(Ready(self.0.take().unwrap()))
        } else {
            Ok(NotReady)
        }
    }
}

struct PeriodicSend<P>(WaitForMessage<P>, Timeout, Box<Fn() -> Protocol<P>>);

impl<P> PeriodicSend<P> {
    fn new<F>(wait: WaitForMessage<P>, dur: Duration, send: F, handle: &Handle) -> PeriodicSend<P>
    where
        F: Fn() -> Protocol<P> + 'static,
    {
        PeriodicSend(wait, Timeout::new(dur, handle), Box::new(send))
    }
}

impl<P> Future for PeriodicSend<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.1.poll().is_err() {
            self.1.reset();

            let con = (self.0).0.as_mut().expect("can not be polled after ready");
            con.send_and_poll(self.2());
        }

        self.0.poll()
    }
}

struct DeviceToDeviceConnectionDataWrapper<P, F>
where
    F: Future<Item = Connection<P>, Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    future: F,
    port: u16,
    addr: SocketAddr,
}

impl<P, F> Future for DeviceToDeviceConnectionDataWrapper<P, F>
where
    F: Future<Item = Connection<P>, Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let con = try_ready!(self.future.poll());

        Ok(Ready((con, self.addr, self.port)))
    }
}

impl<P, F> From<(F, SocketAddr, u16)> for DeviceToDeviceConnectionDataWrapper<P, F>
where
    F: Future<Item = Connection<P>, Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn from(val: (F, SocketAddr, u16)) -> DeviceToDeviceConnectionDataWrapper<P, F> {
        DeviceToDeviceConnectionDataWrapper {
            future: val.0,
            port: val.2,
            addr: val.1,
        }
    }
}

struct DeviceToDeviceConnectionDataWrapperFirst<P, F>
where
    F: Future<Item = (Connection<P>, u16), Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    future: F,
    addr: SocketAddr,
}

impl<P, F> Future for DeviceToDeviceConnectionDataWrapperFirst<P, F>
where
    F: Future<Item = (Connection<P>, u16), Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let con = try_ready!(self.future.poll());

        Ok(Ready((con.0, self.addr, con.1)))
    }
}

impl<P, F> DeviceToDeviceConnectionDataWrapperFirst<P, F>
where
    F: Future<Item = (Connection<P>, u16), Error = Error>,
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn new(f: F, addr: SocketAddr) -> DeviceToDeviceConnectionDataWrapperFirst<P, F> {
        DeviceToDeviceConnectionDataWrapperFirst {
            future: f,
            addr,
        }
    }
}

pub struct DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    wait_for_connect:
        FuturesUnordered<DeviceToDeviceConnectionDataWrapperFirst<P, WaitForConnect<P>>>,
    wait_for_hello: FuturesUnordered<DeviceToDeviceConnectionDataWrapper<P, PeriodicSend<P>>>,
    handle: Handle,
}

impl<P> DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    pub fn new(
        strat: Connector,
        addresses: &[SocketAddr],
        handle: &Handle,
    ) -> DeviceToDeviceConnection<P> {
        let mut strat = strat.get_connect();
        let address: SocketAddr = ([127,0,0,1], addresses.get(0).unwrap().port()).into();
        DeviceToDeviceConnection {
            wait_for_connect: futures_unordered(
                ::std::iter::once(DeviceToDeviceConnectionDataWrapperFirst::new(strat.connect(address).unwrap(), address))
                    /*
                addresses
                    .iter().take(1)
                    .map(|a| DeviceToDeviceConnectionDataWrapperFirst::new(strat.connect(*a).unwrap(), *a))*/,
            ),
            wait_for_hello: FuturesUnordered::new(),
            handle: handle.clone(),
        }
    }
}

impl<P> Future for DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr, u16);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let connection = self.wait_for_connect.poll()?;

            match connection {
                Ready(Some(con)) => {
                    let wait = WaitForMessage::new(con.0, Protocol::Hello);
                    let resend = PeriodicSend::new(
                        wait,
                        Duration::from_millis(100),
                        || Protocol::Hello,
                        &self.handle,
                    );
                    self.wait_for_hello.push((resend, con.1, con.2).into());
                }
                _ => break,
            }
        }

        match try_ready!(self.wait_for_hello.poll()) {
            Some(con) => Ok(Ready(con)),
            None => Ok(NotReady),
        }
    }
}
