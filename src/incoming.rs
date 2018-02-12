use errors::*;
use context;
use protocol::Protocol;
use timeout::Timeout;

use std::time::Duration;

use futures::{Future, Poll};
use futures::Async::{Ready};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForInitialMessage))]
    WaitForStream {
        con: strategies::Connection<P>,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(Finished))]
    WaitingForInitialMessage {
        con: strategies::Connection<P>,
        stream: strategies::Stream<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    Finished(Option<(strategies::Connection<P>, strategies::Stream<P>)>),
    #[state_machine_future(error)]
    ErrorState(Error),
}

impl<P> Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub(crate) fn new(
        con: context::Connection<P>,
        timeout: Duration,
        handle: &Handle,
    ) -> Handler<P> {
        Handler::init(con, Timeout::new(timeout, handle))
    }
}

impl<P> PollConnectStateMachine<P> for Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_wait_for_stream<'a>(wait: &'a mut RentToOwn<'a, WaitForStream>) -> Poll<AfterWaitForStream<P>, Error> {
        let stream = match try_ready!(wait.con.poll()) {
            Some(con) => con,
            None => return Ok(Ready(Finished(None).into()));
        };

        let wait = wait.take();

        Ok(Ready(WaitForInitialMessage{ con: wait.con, timeout: wait.timeout, stream }.into())))
        }

    fn poll_wait_for_initial_message<'a>(wait: &'a mut RentToOwn<'a, WaitForInitialMessage>) -> Poll<AfterWaitForInitialMessage<P>, Error> {
        loop {
            if let Err(e) = wait.timeout.poll() {
                bail!("timeout at incoming::Handler");
            }

            match try_ready!(wait.con.poll()) {
                Some(Protocol::RequestConnection) => {let wait = wait.take(); Ok(Ready(Finished(Some((wait.con, wait.stream)))))},
                Some(Protocol::PokeConnection) => Ok(Ready(Finished(None).into())),
                _ => {}
            }
        }
    }
}
impl<P> Future for Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = context::Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            if let Err(e) = self.timeout.poll() {
                bail!("timeout at incoming::Handler");
            }

            match try_ready!(self.con.as_ref().expect("can not be polled twice")) {
                Some(Protocol::RequestConnection) => Ok(Ready(self.con.take())),
                Some(Protocol::PokeConnection) => Ok(Ready(None)),
                _ => {}
            }
        }
    }
}
