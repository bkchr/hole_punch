use connection::{NewConnectionFuture, NewConnectionHandle};
use context::ResolvePeer;
use error::*;
use protocol::{Protocol, StreamType};
use stream::{NewStreamFuture, Stream, StreamHandle};
use timeout::Timeout;

use std::{net::SocketAddr, time::Duration};

use tokio_core::reactor::Handle;

use futures::{
    stream::{futures_unordered, FuturesUnordered}, sync::oneshot, Async::{NotReady, Ready}, Future,
    Poll, Stream as FStream,
};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum ConnectStateMachine<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    #[state_machine_future(start, transitions(WaitForConnection))]
    InitConnect {
        strat: NewConnectionHandle<P, R>,
        addr: SocketAddr,
        handle: Handle,
    },
    #[state_machine_future(transitions(WaitForConnectStream))]
    WaitForConnection {
        wait: NewConnectionFuture<P, R>,
        timeout: Timeout,
        handle: Handle,
    },
    #[state_machine_future(transitions(ConnectionCreated))]
    WaitForConnectStream {
        wait: NewStreamFuture<P, R>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    ConnectionCreated(Stream<P, R>),
    #[state_machine_future(error)]
    ConnectionError(Error),
}

impl<P, R> PollConnectStateMachine<P, R> for ConnectStateMachine<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn poll_init_connect<'a>(
        init: &'a mut RentToOwn<'a, InitConnect<P, R>>,
    ) -> Poll<AfterInitConnect<P, R>, Error> {
        let mut init = init.take();

        let wait = init.strat.new_connection(init.addr);
        let timeout = Timeout::new(Duration::from_secs(2), &init.handle);
        let handle = init.handle;

        transition!(WaitForConnection { wait, timeout, handle })
    }

    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection<P, R>>,
    ) -> Poll<AfterWaitForConnection<P, R>, Error> {
        let _ = wait.timeout.poll()?;

        let mut con = try_ready!(wait.wait.poll());

        let wait_old = wait.take();
        let timeout = wait_old.timeout.new_reset();

        let wait = con.new_stream();

        wait_old.handle.spawn(con);

        transition!(WaitForConnectStream { timeout, wait })
    }

    fn poll_wait_for_connect_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnectStream<P, R>>,
    ) -> Poll<AfterWaitForConnectStream<P, R>, Error> {
        let _ = wait.timeout.poll()?;

        let mut stream = try_ready!(wait.wait.poll());

        stream.direct_send(Protocol::Hello(None))?;

        transition!(ConnectionCreated(stream))
    }
}

pub struct ConnectWithStrategies<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    strategies: Vec<NewConnectionHandle<P, R>>,
    connect: ConnectStateMachineFuture<P, R>,
    addr: SocketAddr,
    handle: Handle,
}

impl<P, R> ConnectWithStrategies<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub(crate) fn new(
        mut strategies: Vec<NewConnectionHandle<P, R>>,
        handle: &Handle,
        addr: SocketAddr,
    ) -> ConnectWithStrategies<P, R> {
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

impl<P, R> Future for ConnectWithStrategies<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
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

#[derive(StateMachineFuture)]
pub enum PeerToPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    #[state_machine_future(start, transitions(WaitingForStream, ConnectionEstablished))]
    WaitingForConnection {
        timeout: Timeout,
        new_cons: FuturesUnordered<NewConnectionFuture<P, R>>,
        handle: Handle,
    },
    #[state_machine_future(transitions(ConnectionEstablished))]
    WaitingForStream { new_stream: NewStreamFuture<P, R> },
    #[state_machine_future(ready)]
    ConnectionEstablished(Stream<P, R>),
    #[state_machine_future(error)]
    PeerToPeerError(Error),
}

impl<P, R> PeerToPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        mut new_connection_handle: NewConnectionHandle<P, R>,
        peer_addresses: Vec<SocketAddr>,
        handle: &Handle,
    ) -> PeerToPeerConnectionFuture<P, R> {
        let timeout = Timeout::new(Duration::from_secs(20), handle);
        let cons = futures_unordered(
            peer_addresses
                .into_iter()
                .map(|a| new_connection_handle.new_connection(a)),
        );
        PeerToPeerConnection::start(timeout, cons, handle.clone())
    }
}

impl<P, R> PollPeerToPeerConnection<P, R> for PeerToPeerConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn poll_waiting_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForConnection<P, R>>,
    ) -> Poll<AfterWaitingForConnection<P, R>, Error> {
        wait.timeout.poll()?;

        let mut con = match try_ready!(wait.new_cons.poll()) {
            Some(con) => con,
            None => bail!("PeerToPeerConnection new_cons returned `None`."),
        };

        let new_stream = con.new_stream();

        wait.handle.spawn(con);

        transition!(WaitingForStream { new_stream })
    }

    fn poll_waiting_for_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForStream<P, R>>,
    ) -> Poll<AfterWaitingForStream<P, R>, Error> {
        let stream = try_ready!(wait.new_stream.poll());
        transition!(ConnectionEstablished(stream))
    }
}

pub struct RelayConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_stream: NewStreamFuture<P, R>,
    peer: R::Identifier,
}

impl<P, R> RelayConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(peer: R::Identifier, mut stream_handle: StreamHandle<P, R>) -> RelayConnection<P, R> {
        let new_stream = stream_handle.new_stream();
        RelayConnection { new_stream, peer }
    }
}

impl<P, R> Future for RelayConnection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Stream<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut stream = try_ready!(self.new_stream.poll());

        stream.set_p2p(false);
        stream.send_and_poll(StreamType::Relay(self.peer.clone()))?;
        Ok(Ready(stream))
    }
}

#[derive(StateMachineFuture)]
pub enum BuildConnectionToPeer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    #[state_machine_future(start, transitions(TryRelayConnection, ConnectionBuild))]
    TryPeerToPeerConnection {
        peer: R::Identifier,
        stream_handle: StreamHandle<P, R>,
        p2p_con: PeerToPeerConnectionFuture<P, R>,
        result_send: oneshot::Sender<Stream<P, R>>,
    },
    #[state_machine_future(transitions(ConnectionBuild))]
    TryRelayConnection {
        relay_con: RelayConnection<P, R>,
        result_send: oneshot::Sender<Stream<P, R>>,
    },
    #[state_machine_future(ready)]
    ConnectionBuild(()),
    #[state_machine_future(error)]
    ConnectionBuildError(Error),
}

impl<P, R> BuildConnectionToPeer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn create_and_run(
        new_connection_handle: NewConnectionHandle<P, R>,
        peer: R::Identifier,
        peer_addresses: Vec<SocketAddr>,
        stream_handle: StreamHandle<P, R>,
        handle: &Handle,
        result_send: oneshot::Sender<Stream<P, R>>,
    ) {
        let p2p_con = PeerToPeerConnection::new(new_connection_handle, peer_addresses, handle);
        let build = BuildConnectionToPeer::start(peer, stream_handle, p2p_con, result_send);
        handle.spawn(build.map_err(|e| println!("{:?}", e)));
    }
}

impl<P, R> PollBuildConnectionToPeer<P, R> for BuildConnectionToPeer<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn poll_try_peer_to_peer_connection<'a>(
        try: &'a mut RentToOwn<'a, TryPeerToPeerConnection<P, R>>,
    ) -> Poll<AfterTryPeerToPeerConnection<P, R>, Error> {
        let stream = match try.p2p_con.poll() {
            Ok(Ready(stream)) => stream,
            Ok(NotReady) => return Ok(NotReady),
            Err(e) => {
                println!("{:?}", e);

                let try = try.take();
                let relay_con = RelayConnection::new(try.peer, try.stream_handle);
                transition!(TryRelayConnection {
                    relay_con,
                    result_send: try.result_send
                });
            }
        };

        let try = try.take();
        let _ = try.result_send.send(stream);
        transition!(ConnectionBuild(()))
    }

    fn poll_try_relay_connection<'a>(
        try: &'a mut RentToOwn<'a, TryRelayConnection<P, R>>,
    ) -> Poll<AfterTryRelayConnection, Error> {
        let stream = try_ready!(try.relay_con.poll());

        let try = try.take();
        let _ = try.result_send.send(stream);
        transition!(ConnectionBuild(()))
    }
}

fn create_poke_connections<P, R>(
    mut new_connection_handle: NewConnectionHandle<P, R>,
    peer_addresses: Vec<SocketAddr>,
) where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    // Create the connections, we don't want to use them, we just need to open the NAT port.
    peer_addresses.into_iter().for_each(|a| {
        new_connection_handle.new_connection(a);
    });
}

pub fn build_connection_to_peer<P, R>(
    new_connection_handle: NewConnectionHandle<P, R>,
    peer: R::Identifier,
    peer_addresses: Vec<SocketAddr>,
    stream_handle: StreamHandle<P, R>,
    handle: &Handle,
    result_send: Option<oneshot::Sender<Stream<P, R>>>,
) where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    match result_send {
        Some(result_send) => {
            BuildConnectionToPeer::create_and_run(
                new_connection_handle,
                peer,
                peer_addresses,
                stream_handle,
                handle,
                result_send,
            );
        }
        None => {
            create_poke_connections(new_connection_handle, peer_addresses);
        }
    }
}
