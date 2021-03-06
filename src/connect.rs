use crate::{
    connection::{NewConnectionFuture, NewConnectionHandle},
    error::*,
    protocol::StreamHello,
    stream::{NewStreamFuture, Stream},
    timeout::Timeout,
};

use std::{net::SocketAddr, time::Duration};

use futures::{
    Async::{NotReady, Ready},
    Future, Poll,
};

use state_machine_future::{transition, RentToOwn, StateMachineFuture};

use tokio;

#[derive(StateMachineFuture)]
enum ConnectStateMachine {
    #[state_machine_future(start, transitions(WaitForConnection))]
    InitConnect {
        strat: NewConnectionHandle,
        addr: SocketAddr,
        hello_msg: StreamHello,
    },
    #[state_machine_future(transitions(WaitForConnectStream))]
    WaitForConnection {
        wait: NewConnectionFuture,
        timeout: Timeout,
        hello_msg: StreamHello,
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
        let timeout = Timeout::new(Duration::from_secs(4));

        transition!(WaitForConnection {
            wait,
            timeout,
            hello_msg: init.hello_msg
        })
    }

    fn poll_wait_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitForConnection>,
    ) -> Poll<AfterWaitForConnection, Error> {
        let _ = wait.timeout.poll()?;

        let mut con = try_ready!(wait.wait.poll());

        let wait_old = wait.take();
        let timeout = wait_old.timeout.new_reset();

        let wait = con.new_stream_with_hello(wait_old.hello_msg);

        tokio::spawn(con);

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
    hello_msg: StreamHello,
}

impl ConnectWithStrategies {
    pub(crate) fn new(
        mut strategies: Vec<NewConnectionHandle>,
        addr: SocketAddr,
        hello_msg: StreamHello,
    ) -> ConnectWithStrategies {
        let strategy = strategies
            .pop()
            .expect("At least one strategy should be given!");

        ConnectWithStrategies {
            strategies,
            addr,
            connect: ConnectStateMachine::start(strategy, addr, hello_msg.clone()),
            hello_msg,
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
                error!("error: {:?}", e);

                match self.strategies.pop() {
                    Some(strat) => {
                        self.connect =
                            ConnectStateMachine::start(strat, self.addr, self.hello_msg.clone());
                        let _ = self.connect.poll()?;
                        Ok(NotReady)
                    }
                    None => bail!("No strategies left for connecting to: {}", self.addr),
                }
            }
        }
    }
}
