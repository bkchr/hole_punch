use context;
use error::*;
use protocol::Protocol;
use timeout::Timeout;

use std::time::Duration;

use futures::{Future, Poll, Stream};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForInitialMessage, Finished))]
    WaitForStream {
        con: context::Connection<P>,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(Finished))]
    WaitForInitialMessage {
        con: context::Connection<P>,
        stream: context::Stream<P>,
        timeout: Timeout,
    },
    #[state_machine_future(ready)]
    Finished(
        Option<(
            context::Connection<P>,
            context::Stream<P>,
            Option<context::ConnectionId>,
        )>,
    ),
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
    ) -> HandlerFuture<P> {
        Handler::start(con, Timeout::new(timeout, handle))
    }
}

impl<P> PollHandler<P> for Handler<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_wait_for_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitForStream<P>>,
    ) -> Poll<AfterWaitForStream<P>, Error> {
        let stream = match try_ready!(wait.con.poll()) {
            Some(con) => con,
            None => transition!(Finished(None)),
        };

        let wait = wait.take();

        transition!(WaitForInitialMessage {
            con: wait.con,
            timeout: wait.timeout,
            stream,
        })
    }

    fn poll_wait_for_initial_message<'a>(
        wait: &'a mut RentToOwn<'a, WaitForInitialMessage<P>>,
    ) -> Poll<AfterWaitForInitialMessage<P>, Error> {
        loop {
            if let Err(_) = wait.timeout.poll() {
                bail!("timeout at incoming::Handler");
            }

            match try_ready!(wait.stream.direct_poll()) {
                Some(Protocol::RequestConnection) => {
                    let mut wait = wait.take();
                    wait.stream.direct_send(Protocol::ConnectionEstablished);
                    transition!(Finished(Some((wait.con, wait.stream, None))))
                }
                Some(Protocol::PokeConnection) => {
                    println!("POKE");
                    wait.stream.direct_send(Protocol::ConnectionEstablished);
                    transition!(Finished(None))
                }
                Some(Protocol::PeerToPeerConnection(connection_id)) => {
                    println!("PEERTOPEER");
                    let mut wait = wait.take();
                    wait.stream.direct_send(Protocol::ConnectionEstablished);
                    transition!(Finished(Some((wait.con, wait.stream, Some(connection_id)))))
                }
                _ => {}
            }
        }
    }
}
