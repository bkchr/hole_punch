use connection::{NewConnectionFuture, NewConnectionHandle};
use error::*;
use protocol::StreamHello;
use stream::{NewStreamFuture, Stream};
use timeout::Timeout;
use PubKeyHash;

use std::{net::SocketAddr, time::Duration};

use tokio_core::reactor::Handle;

use futures::{
    Async::{NotReady, Ready},
    Future, Poll,
};

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
enum ConnectStateMachine {
    #[state_machine_future(start, transitions(WaitForConnection))]
    InitConnect {
        strat: NewConnectionHandle,
        addr: SocketAddr,
        handle: Handle,
        local_peer_identifier: PubKeyHash,
    },
    #[state_machine_future(transitions(WaitForConnectStream))]
    WaitForConnection {
        wait: NewConnectionFuture,
        timeout: Timeout,
        handle: Handle,
        local_peer_identifier: PubKeyHash,
    },
    #[state_machine_future(transitions(ConnectionCreated))]
    WaitForConnectStream {
        wait: NewStreamFuture,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    ConnectionCreated(Stream),
    #[state_machine_future(error)]
    ConnectionError(Error),
}

impl PollConnectStateMachine for ConnectStateMachine {
    fn poll_init_connect<'a>(
        init: &'a mut RentToOwn<'a, InitConnect>,
    ) -> Poll<AfterInitConnect, Error> {
        let mut init = init.take();

        let wait = init.strat.new_connection(init.addr);
        let timeout = Timeout::new(Duration::from_secs(2), &init.handle);
        let handle = init.handle;

        transition!(WaitForConnection {
            wait,
            timeout,
            handle,
            local_peer_identifier: init.local_peer_identifier,
        })
    }

    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection>,
    ) -> Poll<AfterWaitForConnection, Error> {
        let _ = wait.timeout.poll()?;

        let mut con = try_ready!(wait.wait.poll());

        let wait_old = wait.take();
        let timeout = wait_old.timeout.new_reset();

        let wait = con.new_stream_with_hello(StreamHello::User(wait_old.local_peer_identifier));

        wait_old.handle.spawn(con);

        transition!(WaitForConnectStream { timeout, wait })
    }

    fn poll_wait_for_connect_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnectStream>,
    ) -> Poll<AfterWaitForConnectStream, Error> {
        let _ = wait.timeout.poll()?;

        let stream = try_ready!(wait.wait.poll());

        transition!(ConnectionCreated(stream))
    }
}

pub struct ConnectWithStrategies {
    strategies: Vec<NewConnectionHandle>,
    connect: ConnectStateMachineFuture,
    addr: SocketAddr,
    handle: Handle,
    local_peer_identifier: PubKeyHash,
}

impl ConnectWithStrategies {
    pub(crate) fn new(
        mut strategies: Vec<NewConnectionHandle>,
        handle: Handle,
        addr: SocketAddr,
        local_peer_identifier: PubKeyHash,
    ) -> ConnectWithStrategies {
        let strategy = strategies
            .pop()
            .expect("At least one strategy should be given!");

        ConnectWithStrategies {
            strategies,
            addr,
            connect: ConnectStateMachine::start(
                strategy,
                addr,
                handle.clone(),
                local_peer_identifier.clone(),
            ),
            handle,
            local_peer_identifier,
        }
    }
}

impl Future for ConnectWithStrategies {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.connect.poll() {
            Ok(Ready(con)) => Ok(Ready(con)),
            Ok(NotReady) => Ok(NotReady),
            Err(e) => {
                eprintln!("error: {:?}", e);

                match self.strategies.pop() {
                    Some(mut strat) => {
                        self.connect = ConnectStateMachine::start(
                            strat,
                            self.addr,
                            self.handle.clone(),
                            self.local_peer_identifier.clone(),
                        );
                        let _ = self.connect.poll()?;
                        Ok(NotReady)
                    }
                    None => bail!("No strategies left for connecting to: {}", self.addr),
                }
            }
        }
    }
}
