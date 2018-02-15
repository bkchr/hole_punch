use errors::*;
use protocol::Protocol;
use context::{Connection, ConnectionId, NewConnectionFuture, NewConnectionHandle, NewStreamFuture,
              NewStreamHandle, Stream};
use timeout::Timeout;

use std::net::SocketAddr;
use std::time::Duration;
use std::mem::discriminant;

use tokio_core::reactor::{self, Handle};

use futures::{Future, Join, Poll, Sink};
use futures::stream::StreamFuture;
use futures::Async::{NotReady, Ready};
use futures::stream::{futures_unordered, FuturesUnordered};
use futures::sync::{mpsc, oneshot};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

use either::{self, Either};

#[derive(StateMachineFuture)]
pub enum ConnectStateMachine<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForConnection))]
    InitConnect {
        strat: NewConnectionHandle,
        addr: SocketAddr,
        handle: Handle,
    },
    #[state_machine_future(transitions(WaitForConnectStream))]
    WaitForConnection {
        wait: NewConnectionFuture<P>,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(WaitForInitialAnswer))]
    WaitForConnectStream {
        con: Connection<P>,
        wait: NewStreamFuture<P>,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(ConnectionCreated))]
    WaitForInitialAnswer {
        wait: WaitForMessage<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    ConnectionCreated((Connection<P>, Stream<P>)),
    #[state_machine_future(error)]
    ConnectionError(Error),
}

impl<P> PollConnectStateMachine<P> for ConnectStateMachine<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_init_connect<'a>(
        init: &'a mut RentToOwn<'a, InitConnect>,
    ) -> Poll<AfterInitConnect<P>, Error> {
        let init = init.take();

        let wait = init.strat.new_connection(init.addr);
        let timeout = Timeout::new(Duration::from_secs(2), init.handle);

        transition!(WaitForConnection { wait, timeout })
    }

    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection<P>>,
    ) -> Poll<AfterWaitForConnection<P>, Error> {
        let _ = wait.timeout().poll()?;

        let mut con = try_ready!(wait.wait.poll());

        let wait = wait.take();
        let timeout = wait.timeout.new_reset();

        let wait = con.new_stream();

        transition!(WaitForConnectStream { con, timeout, wait })
    }

    fn poll_wait_for_connect_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnectStream<P>>,
    ) -> Poll<AfterWaitForConnectStream<P>, Error> {
        let _ = wait.timeout().poll()?;

        let mut stream = try_ready!(wait.wait.poll());

        stream.send_and_poll(Protocol::RequestConnection);

        let wait = wait.take();
        let timeout = wait.timeout.new_reset();
        let wait = WaitForMessage::new(Some(wait.con), stream, Protocol::ConnectionEstablished);

        transition!(WaitForRelayMode { wait, timeout })
    }

    fn poll_wait_for_initial_answer<'a>(
        wait: &'a mut RentToOwn<'a, WaitForInitialAnswer<P>>,
    ) -> Poll<AfterWaitForInitialAnswer<P>, Error> {
        let _ = wait.timeout().poll()?;

        let (con, stream) = try_ready!(wait.wait.poll());

        transition!(ConnectionCreated((con.unwrap(), stream)))
    }
}

pub struct ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    strategies: Vec<NewConnectionHandle>,
    connect: ConnectStateMachineFuture<P>,
    addr: SocketAddr,
    handle: Handle,
}

impl<P> ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub(crate) fn new(
        mut strategies: Vec<NewConnectionHandle>,
        handle: &Handle,
        addr: SocketAddr,
    ) -> ConnectWithStrategies<P> {
        let strategy = strategies
            .pop()
            .expect("At least one strategy should be given!");

        ConnectWithStrategies {
            strategies,
            addr,
            connect: ConnectStateMachine::start(strategy, addr, handle.clone()),
            handle: handle.clone(),
        }
    }
}

impl<P> Future for ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ( Connection<P>, Stream<P> );
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

pub struct WaitForMessage<P>(Option<Connection<P>>, Option<Stream<P>>, Protocol<P>);

impl<P> WaitForMessage<P> {
    pub fn new(
        con: Option<Connection<P>>,
        stream: Stream<P>,
        msg: Protocol<P>,
    ) -> WaitForMessage<P> {
        WaitForMessage(Some(con), Some(stream), msg)
    }
}

impl<P> Future for WaitForMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (Option<Connection<P>>, Stream<P>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let message = match try_ready!(
                self.1
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

struct WaitForNewStream<P> {
    new_stream: NewStreamFuture<P>,
    con: Option<Connection<P>>,
}

impl<P> WaitForNewStream<P> {
    fn new(con: Connection<P>) -> WaitForNewStream<P> {
        let new_stream = con.new_stream();
        let con = Some(con);
        WaitForNewStream { new_stream, con }
    }
}

impl<P> Future for WaitForNewStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (Connection<P>, Stream<P>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_stream
            .poll()
            .map(|r| r.map(|v| (self.con.take().expect("can only be taken once"), v)))
    }
}

pub struct DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    wait_for_con: FuturesUnordered<NewConnectionFuture<P>>,
    wait_for_stream: FuturesUnordered<WaitForNewStream<P>>,
    wait_for_resp: FuturesUnordered<WaitForMessage<P>>,
    handle: Handle,
    is_master: bool,
    connection_id: ConnectionId,
}

impl<P> DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        strat: NewConnectionHandle,
        addresses: &[SocketAddr],
        handle: &Handle,
    ) -> DirectDeviceToDeviceConnection<P> {
        let mut strat = strat.get_connect();
        DirectDeviceToDeviceConnection {
            wait_for_con: futures_unordered(addresses.iter().map(|a| strat.new_connection(*a))),
            wait_for_stream: FuturesUnordered::new(),
            wait_for_resp: FuturesUnordered::new(),
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
        loop {
            let con = self.wait_for_con.poll()?;

            match con {
                Ready(Some(con)) => self.wait_for_stream.push(WaitForNewStream::new(con)),
                _ => break,
            }
        }

        loop {
            let res = self.wait_for_stream.poll()?;

            match res {
                Ready(Some((con, mut stream))) => {
                    if self.is_master {
                        stream.direct_send(Protocol::PokeConnection).unwrap();
                    } else {
                        stream
                            .direct_send(Protocol::PeerToPeerConnection(self.connection_id))
                            .unwrap();
                    }

                    self.wait_for_resp.push(WaitForNewStream::new(
                        Some(con),
                        stream,
                        Protocol::ConnectionEstablished,
                    ));
                }
                _ => break,
            }
        }

        loop {
            match self.wait_for_resp.poll() {
                Ok(Ready(Some(con))) => return Ok(Ready(con)),
                Ok(NotReady) => return Ok(NotReady),
                Ok(Ready(None)) => {
                    if self.wait_for_stream.is_empty() {
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
    WaitForStream {
        wait: NewStreamFuture<P>,
        other_device_id: u64,
    },
    #[state_machine_future(transitions(RelayModeActivated))]
    WaitForRelayMode { wait: WaitForMessage<P> },
    #[state_machine_future(ready)]
    RelayModeActivated(Connection<P>),
    #[state_machine_future(error)]
    RelayModeError(Error),
}

impl<P> PollRelayDeviceToDeviceConnection<P> for RelayDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_wait_for_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForStream<P>>,
    ) -> Poll<AfterWaitForStream<P>, Error> {
        let mut stream = try_ready!(wait.wait.poll());

        stream.send_and_poll(Protocol::RelayConnection(wait.other_device_id));

        let wait = WaitForMessage::new(None, stream, Protocol::RelayModeActivated);

        transition!(WaitForRelayMode { wait })
    }

    fn poll_wait_for_relay_mode<'a>(
        wait: &'a mut RentToOwn<'a, WaitForRelayMode<P>>,
    ) -> Poll<AfterWaitForRelayMode<P>, Error> {
        let con = try_ready!(wait.wait.poll());

        transition!(RelayModeActivated(con))
    }
}

#[derive(StateMachineFuture)]
pub enum DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(TryDirectConnection))]
    InitState {
        new_connection_handle: NewConnectionHandle,
        addresses: Vec<SocketAddr>,
        new_stream_handle: NewStreamHandle,
        connection_id: ConnectionId,
        is_master: bool,
        handle: Handle,
    },
    #[state_machine_future(transitions(ConnectionEstablished, RelayConnection))]
    TryDirectConnection {
        connect: DirectDeviceToDeviceConnection<P>,
        timeout: Timeout,
        new_stream_handle: NewStreamHandle,
        connection_id: ConnectionId,
        is_master: bool,
    },
    #[state_machine_future(transitions(ConnectionEstablished))]
    RelayConnection {
        relay: RelayDeviceToDeviceConnectionFuture<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    ConnectionEstablished(Either<(Connection<P>, Stream<P>), Stream<P>>),
    #[state_machine_future(error)]
    ErrorState(Error),
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
        let connection_id = init.connection_id;
        let is_master = init.is_master;
        let new_stream_handle = init.new_stream_handle;
        let new_connection_handle = init.new_connection_handle;

        transition!(TryDirectConnection {
            connect: DirectDeviceToDeviceConnection::new(
                new_connection_handle,
                &init.addresses,
                &init.handle,
            ),
            timeout,
            new_stream_handle,
            connection_id,
            is_master,
        })
    }

    fn poll_try_direct_connection<'a>(
        try: &'a mut RentToOwn<'a, TryDirectConnection<P>>,
    ) -> Poll<AfterTryDirectConnection<P>, Error> {
        let con = try.connect.poll();
        let timeout = try.timeout.poll();

        if timeout.is_err() || con.is_err() {
            if try.is_master {
                unimplemented!();
                let try = try.take();

                let timeout = try.timeout.new_reset();

                transition!(RelayConnection {
                    relay: RelayDeviceToDeviceConnection::start(
                        try.new_stream_handle.new_session(),
                        try.connection_id,
                    ),
                    timeout,
                })
            } else {
                // The controller will create a relay connection via the server
                bail!("direct device to device connection timeout")
            }
        } else {
            let con = try_ready!(con);

            transition!(ConnectionEstablished(con))
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

        transition!(ConnectionEstablished(con))
    }
}

pub struct ConnectToPeerCoordinator<P> {
    connection_id: ConnectionId,
    recv: Join<oneshot::Receiver<Vec<SocketAddr>>, oneshot::Receiver<Vec<SocketAddr>>>,
    master: mpsc::UnboundedSender<Protocol<P>>,
    slave: mpsc::UnboundedReceiver<Protocol<P>>,
}

impl<P> ConnectToPeerCoordinator<P> {
    pub fn spawn(
        handle: &Handle,
        connection_id: ConnectionId,
        master: mpsc::Sender<Protocol<P>>,
        slave: mpsc::Sender<Protocol<P>>,
    ) -> (ConnectToPeerHandle, ConnectToPeerHandle) {
        let (master_send, master_recv) = oneshot::channel();
        let (slave_send, slave_recv) = oneshot::channel();

        let mut coordinator = ConnectToPeerCoordinator {
            connection_id,
            recv: master_recv.join(slave_recv),
            master,
            slave,
        };
        coordinator.send_address_request();
        handle.spawn(coordinator.map_err(|e| error!("{:?}", e)));

        (
            ConnectToPeerHandle {
                sender: master_send,
            },
            ConnectToPeerHandle { sender: slave_send },
        )
    }

    fn send_address_request(&mut self) {
        self.master
            .unbounded_send(Protocol::RequestPrivateAdressInformation(
                self.connection_id,
            ));
        self.slave
            .unbounded_send(Protocol::RequestPrivateAdressInformation(
                self.connection_id,
            ));
    }
}

impl<P> Future for ConnectToPeerCoordinator<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (master, slave) = try_ready!(self.recv.poll());

        self.master
            .unbounded_send(Protocol::Connect(slave, 0, self.connection_id));
        self.slave
            .unbounded_send(Protocol::Connect(master, 0, self.connection_id));

        Ok(())
    }
}

pub struct ConnectToPeerHandle {
    sender: oneshot::Sender<Vec<SocketAddr>>,
}

impl ConnectToPeerHandle {
    pub fn send_address_information(self, info: Vec<SocketAddr>) {
        self.sender.send((self.is_master, info));
    }
}
