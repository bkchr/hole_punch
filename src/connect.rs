use context::{Connection, ConnectionId, NewConnectionFuture, NewConnectionHandle};
use error::*;
use protocol::Protocol;
use stream::{NewStreamFuture, Stream, StreamHandle};
use timeout::Timeout;

use std::mem::discriminant;
use std::net::SocketAddr;
use std::time::Duration;

use tokio_core::reactor::Handle;

use futures::stream::{futures_unordered, FuturesUnordered};
use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream as FStream};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum ConnectStateMachine<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForConnection))]
    InitConnect {
        strat: NewConnectionHandle<P>,
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
        init: &'a mut RentToOwn<'a, InitConnect<P>>,
    ) -> Poll<AfterInitConnect<P>, Error> {
        let mut init = init.take();

        let wait = init.strat.new_connection(init.addr);
        let timeout = Timeout::new(Duration::from_secs(2), &init.handle);

        transition!(WaitForConnection { wait, timeout })
    }

    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection<P>>,
    ) -> Poll<AfterWaitForConnection<P>, Error> {
        let _ = wait.timeout.poll()?;

        let mut con = try_ready!(wait.wait.poll());

        let wait = wait.take();
        let timeout = wait.timeout.new_reset();

        let wait = con.new_stream();

        transition!(WaitForConnectStream { con, timeout, wait })
    }

    fn poll_wait_for_connect_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnectStream<P>>,
    ) -> Poll<AfterWaitForConnectStream<P>, Error> {
        let _ = wait.timeout.poll()?;

        let mut stream = try_ready!(wait.wait.poll());

        stream.direct_send(Protocol::RequestConnection)?;

        let wait = wait.take();
        let timeout = wait.timeout.new_reset();
        let wait = WaitForMessage::new(Some(wait.con), stream, Protocol::ConnectionEstablished);

        transition!(WaitForInitialAnswer { wait, timeout })
    }

    fn poll_wait_for_initial_answer<'a>(
        wait: &'a mut RentToOwn<'a, WaitForInitialAnswer<P>>,
    ) -> Poll<AfterWaitForInitialAnswer<P>, Error> {
        let _ = wait.timeout.poll()?;

        let (con, stream) = try_ready!(wait.wait.poll());

        transition!(ConnectionCreated((con.unwrap(), stream)))
    }
}

pub struct ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    strategies: Vec<NewConnectionHandle<P>>,
    connect: ConnectStateMachineFuture<P>,
    addr: SocketAddr,
    handle: Handle,
}

impl<P> ConnectWithStrategies<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub(crate) fn new(
        mut strategies: Vec<NewConnectionHandle<P>>,
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
    type Item = (Connection<P>, Stream<P>);
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

pub struct WaitForMessage<P>(Option<Connection<P>>, Option<Stream<P>>, Protocol<P>)
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone;

impl<P> WaitForMessage<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        con: Option<Connection<P>>,
        stream: Stream<P>,
        msg: Protocol<P>,
    ) -> WaitForMessage<P> {
        WaitForMessage(con, Some(stream), msg)
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
                    .direct_poll()
            ) {
                Some(message) => message,
                None => bail!("connection closed while waiting for Message"),
            };

            if discriminant(&self.2) == discriminant(&message) {
                return Ok(Ready((self.0.take(), self.1.take().unwrap())));
            }
        }
    }
}

struct WaitForNewStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    new_stream: NewStreamFuture<P>,
    con: Option<Connection<P>>,
}

impl<P> WaitForNewStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(mut con: Connection<P>) -> WaitForNewStream<P> {
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
        mut strat: NewConnectionHandle<P>,
        addresses: &[SocketAddr],
        handle: &Handle,
        is_master: bool,
        connection_id: ConnectionId,
    ) -> DirectDeviceToDeviceConnection<P> {
        DirectDeviceToDeviceConnection {
            wait_for_con: futures_unordered(addresses.iter().map(|a| strat.new_connection(*a))),
            wait_for_stream: FuturesUnordered::new(),
            wait_for_resp: FuturesUnordered::new(),
            handle: handle.clone(),
            is_master,
            connection_id,
        }
    }
}

impl<P> Future for DirectDeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (Connection<P>, Stream<P>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let con = self.wait_for_con.poll()?;

            match con {
                Ready(Some(mut con)) => {
                    con.set_p2p(true);
                    self.wait_for_stream.push(WaitForNewStream::new(con));
                }
                _ => break,
            }
        }

        loop {
            let res = self.wait_for_stream.poll()?;

            match res {
                Ready(Some((con, mut stream))) => {
                    if self.is_master {
                        stream
                            .direct_send(Protocol::PeerToPeerConnection(self.connection_id))
                            .unwrap();
                    } else {
                        stream.direct_send(Protocol::PokeConnection).unwrap();
                    }

                    self.wait_for_resp.push(WaitForMessage::new(
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
                Ok(Ready(Some((con, mut stream)))) => {
                    stream.direct_send(Protocol::ConnectionSelected).unwrap();
                    return Ok(Ready((con.unwrap(), stream)));
                }
                Ok(NotReady) => return Ok(NotReady),
                Ok(Ready(None)) => {
                    if self.wait_for_stream.is_empty() && self.wait_for_con.is_empty() {
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
pub enum DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(TryDirectConnection))]
    InitState {
        new_connection_handle: NewConnectionHandle<P>,
        addresses: Vec<SocketAddr>,
        stream_handle: StreamHandle<P>,
        connection_id: ConnectionId,
        is_master: bool,
        handle: Handle,
    },
    #[state_machine_future(transitions(ConnectionEstablished))]
    TryDirectConnection {
        connect: DirectDeviceToDeviceConnection<P>,
        timeout: Timeout,
        stream_handle: StreamHandle<P>,
        connection_id: ConnectionId,
        is_master: bool,
    },
    #[state_machine_future(ready)]
    ConnectionEstablished((Option<Connection<P>>, Option<Stream<P>>, ConnectionId)),
    #[state_machine_future(error)]
    ErrorState(Error),
}

impl<P> PollDeviceToDeviceConnection<P> for DeviceToDeviceConnection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_init_state<'a>(
        init: &'a mut RentToOwn<'a, InitState<P>>,
    ) -> Poll<AfterInitState<P>, Error> {
        let init = init.take();

        let timeout = Timeout::new(
            Duration::from_secs(if init.is_master { 20 } else { 1 }),
            &init.handle,
        );
        let connection_id = init.connection_id;
        let is_master = init.is_master;
        let new_connection_handle = init.new_connection_handle;

        transition!(TryDirectConnection {
            connect: DirectDeviceToDeviceConnection::new(
                new_connection_handle,
                &init.addresses,
                &init.handle,
                is_master,
                connection_id,
            ),
            timeout,
            stream_handle: init.stream_handle,
            connection_id,
            is_master,
        })
    }

    fn poll_try_direct_connection<'a>(
        try: &'a mut RentToOwn<'a, TryDirectConnection<P>>,
    ) -> Poll<AfterTryDirectConnection<P>, Error> {
        let res = try.connect.poll();
        let timeout = try.timeout.poll();

        if timeout.is_err() || res.is_err() {
            if try.is_master {
                let mut try = try.take();
                try.stream_handle
                    .send_msg(Protocol::RequestRelayPeerConnection(try.connection_id));

                transition!(ConnectionEstablished((None, None, try.connection_id)))
            } else {
                // The controller will create a relay connection via the server
                bail!("direct device to device connection timeout")
            }
        } else {
            let (con, stream) = try_ready!(res);

            transition!(ConnectionEstablished((
                Some(con),
                Some(stream),
                try.connection_id
            )))
        }
    }
}
