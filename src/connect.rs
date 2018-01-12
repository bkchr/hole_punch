use errors::*;
use protocol::Protocol;
use strategies::{Connect, Connection, NewSessionController, NewSessionWait, WaitForConnect};
use timeout::Timeout;

use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::mem::discriminant;

use tokio_core::reactor::{self, Handle};

use futures::{Future, Poll, Sink, Stream};
use futures::Async::{NotReady, Ready};
use futures::stream::{futures_unordered, FuturesUnordered};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
enum ConnectStateMachine<P: 'static + Serialize + for<'de> Deserialize<'de> + Clone> {
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
        timeout: Timeout,
    },
    #[state_machine_future(ready)] Connected(Connection<P>),
    #[state_machine_future(error)] ErrorState(Error),
}

impl<P> PollConnectStateMachine<P> for ConnectStateMachine<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
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
        if wait.timeout.poll().is_err() {
            bail!("wait for connect timeout")
        };
        let mut connection = try_ready!(wait.connect.poll());

        let wait = wait.take();
        connection.send_and_poll(Protocol::Register);

        Ok(Ready(
            WaitingForRegisterResponse {
                wait_for_message: WaitForMessage::new(connection, Protocol::Acknowledge),
                timeout: wait.timeout.new_reset(),
            }.into(),
        ))
    }

    fn poll_waiting_for_register_response<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForRegisterResponse<P>>,
    ) -> Poll<AfterWaitingForRegisterResponse<P>, Error> {
        if wait.timeout.poll().is_err() {
            bail!("wait for register response")
        };

        let connection = try_ready!(wait.wait_for_message.poll());

        Ok(Ready(Connected(connection).into()))
    }
}

pub struct ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    strategies: Vec<Connect>,
    connect: ConnectStateMachineFuture<P>,
    addr: SocketAddr,
    handle: Handle,
}

impl<'connect, P> ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
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
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.connect.poll() {
            Ok(Ready(con)) => Ok(Ready(con)),
            Ok(NotReady) => Ok(NotReady),
            Err(e) => {
                eprintln!("error: {:?}", e);

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
        P: Serialize + for<'de> Deserialize<'de> + Clone,
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
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
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
                return Ok(Ready(self.0.take().unwrap()));
            }
        }
    }
}

pub struct DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    wait_for_connect: FuturesUnordered<WaitForConnect<P>>,
    wait_for_hello: FuturesUnordered<WaitForMessage<P>>,
    handle: Handle,
}

impl<P> DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        strat: Connector,
        addresses: &[SocketAddr],
        handle: &Handle,
    ) -> DirectDeviceToDeviceConnection<P> {
        let mut strat = strat.get_connect();
        DirectDeviceToDeviceConnection {
            wait_for_connect: futures_unordered(
                addresses.iter().map(|a| strat.connect(*a).unwrap()),
            ),
            wait_for_hello: FuturesUnordered::new(),
            handle: handle.clone(),
        }
    }
}

impl<P> Future for DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        bail!("TEST");
        loop {
            let connection = self.wait_for_connect.poll()?;

            match connection {
                Ready(Some(mut con)) => {
                    con.send_and_poll(Protocol::Hello);
                    let wait = WaitForMessage::new(con, Protocol::Hello);
                    self.wait_for_hello.push(wait);
                }
                _ => break,
            }
        }

        //TODO: if a device is reachable via multiple ip addresses, we can get into trouble here,
        //because each side does not know which connection is the correct one and so,
        //both devices could talk via the wrong connection to each other.
        loop {
            match self.wait_for_hello.poll() {
                Ok(Ready(Some(con))) => return Ok(Ready(con)),
                Ok(NotReady) => return Ok(NotReady),
                Ok(Ready(None)) => {
                    if self.wait_for_connect.is_empty() {
                        bail!("No connections left for connecting to device!");
                    } else {
                        return Ok(NotReady);
                    }
                }
                Err(e) => {
                    eprintln!("{:?}", e);
                }
            }
        }
    }
}

#[derive(StateMachineFuture)]
pub enum RelayDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForRelayMode))]
    WaitForNewSession {
        wait: NewSessionWait<P>,
        other_device_id: u64,
    },
    #[state_machine_future(transitions(RelayModeActivated))]
    WaitForRelayMode {
        wait: WaitForMessage<P>,
    },
    #[state_machine_future(ready)] RelayModeActivated(Connection<P>),
    #[state_machine_future(error)] RelayModeError(Error),
}

impl<P> PollRelayDeviceToDeviceConnection<P> for RelayDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_wait_for_new_session<'a>(
        wait: &'a mut RentToOwn<'a, WaitForNewSession<P>>,
    ) -> Poll<AfterWaitForNewSession<P>, Error> {
        let mut con = try_ready!(wait.wait.poll());

        con.send_and_poll(Protocol::RelayConnection(wait.other_device_id));

        let wait = WaitForMessage::new(con, Protocol::RelayModeActivated);

        Ok(Ready(WaitForRelayMode { wait }.into()))
    }

    fn poll_wait_for_relay_mode<'a>(
        wait: &'a mut RentToOwn<'a, WaitForRelayMode<P>>,
    ) -> Poll<AfterWaitForRelayMode<P>, Error> {
        let con = try_ready!(wait.wait.poll());

        Ok(Ready(RelayModeActivated(con).into()))
    }
}

#[derive(StateMachineFuture)]
pub enum DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(TryDirectConnection))]
    InitState {
        connector: Connector,
        addresses: Vec<SocketAddr>,
        new_session_controller: NewSessionController,
        // TODO: This is the id of the device in the hashmap on the server. Don't do that!
        remote_device_id: u64,
        // Is that the device that initiated the connection?
        is_controller: bool,
        handle: Handle,
    },
    #[state_machine_future(transitions(ConnectionEstablished, RelayConnection))]
    TryDirectConnection {
        connect: DirectDeviceToDeviceConnection<P>,
        timeout: Timeout,
        new_session_controller: NewSessionController,
        // TODO: This is the id of the device in the hashmap on the server. Don't do that!
        remote_device_id: u64,
        // Is that the device that initiated the connection?
        is_controller: bool,
    },
    #[state_machine_future(transitions(ConnectionEstablished))]
    RelayConnection {
        relay: RelayDeviceToDeviceConnectionFuture<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)] ConnectionEstablished(Connection<P>),
    #[state_machine_future(error)] ErrorState2(Error),
}

impl<P> PollDeviceToDeviceConnection<P> for DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_init_state<'a>(
        init: &'a mut RentToOwn<'a, InitState>,
    ) -> Poll<AfterInitState<P>, Error> {
        let init = init.take();

        let timeout = Timeout::new(Duration::from_secs(2), &init.handle);
        let remote_device_id = init.remote_device_id;
        let is_controller = init.is_controller;
        let new_session_controller = init.new_session_controller;
        let connector = init.connector;

        Ok(Ready(
            TryDirectConnection {
                connect: DirectDeviceToDeviceConnection::new(
                    connector,
                    &init.addresses,
                    &init.handle,
                ),
                timeout,
                new_session_controller,
                remote_device_id,
                is_controller,
            }.into(),
        ))
    }

    fn poll_try_direct_connection<'a>(
        try: &'a mut RentToOwn<'a, TryDirectConnection<P>>,
    ) -> Poll<AfterTryDirectConnection<P>, Error> {
        let con = try.connect.poll();
        let timeout = try.timeout.poll();

        if timeout.is_err() || con.is_err() {
            if try.is_controller {
                let try = try.take();

                let timeout = try.timeout.new_reset();

                Ok(Ready(
                    RelayConnection {
                        relay: RelayDeviceToDeviceConnection::start(
                            try.new_session_controller.new_session(),
                            try.remote_device_id,
                        ),
                        timeout,
                    }.into(),
                ))
            } else {
                // The controller will create a relay connection via the server
                bail!("direct device to device connection timeout")
            }
        } else {
            let con = try_ready!(con);

            Ok(Ready(ConnectionEstablished(con).into()))
        }
    }

    fn poll_relay_connection<'a>(
        relay: &'a mut RentToOwn<'a, RelayConnection<P>>,
    ) -> Poll<AfterRelayConnection<P>, Error> {
        relay
            .timeout
            .poll()
            .map_err(|_| "relay connection timeout")?;

        let con = try_ready!(relay.relay.poll());

        Ok(Ready(ConnectionEstablished(con).into()))
    }
}
