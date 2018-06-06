use error::*;
use protocol::{ConnectionType, Protocol, StreamType};
use stream::Stream;
use timeout::Timeout;

use std::time::Duration;

use futures::{Future, Poll, Stream as FStream};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitingForSelectedMessage, Finished))]
    WaitingForInitialMessage {
        stream: Stream<P>,
        // Is this the first stream of the `Connection`?
        first_stream: bool,
        timeout: Timeout,
    },
    #[state_machine_future(transitions(Finished))]
    WaitingForSelectedMessage { stream: Stream<P>, timeout: Timeout },
    #[state_machine_future(ready)]
    Finished(Option<Stream<P>>),
    #[state_machine_future(error)]
    ErrorState(Error),
}

impl<P> IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub(crate) fn new(
        stream: Stream<P>,
        first_stream: bool,
        timeout: Duration,
        handle: &Handle,
    ) -> IncomingStreamFuture<P> {
        IncomingStream::start(stream, first_stream, Timeout::new(timeout, handle))
    }
}

impl<P> PollIncomingStream<P> for IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_waiting_for_initial_message<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForInitialMessage<P>>,
    ) -> Poll<AfterWaitingForInitialMessage<P>, Error> {
        if let Err(_) = wait.timeout.poll() {
            bail!("timeout at IncomingStream::waiting_for_initial_message");
        }

        let msg = try_ready!(wait.stream.direct_poll());

        if wait.first_stream {
            match msg {
                Some(Protocol::ConnectionHello(ctype)) => match ctype {
                    None => {
                        let mut wait = wait.take();
                        wait.stream.direct_send(Protocol::ConnectionHelloAck)?;
                        transition!(Finished(Some(wait.stream)))
                    }
                    Some(ConnectionType::PeerToPeerPoke) => {
                        println!("POKE");
                        wait.stream.direct_send(Protocol::ConnectionHelloAck)?;
                        transition!(Finished(None))
                    }
                    Some(ConnectionType::PeerToPeer(id)) => {
                        println!("PEERTOPEER");
                        let mut wait = wait.take();
                        wait.stream.direct_send(Protocol::ConnectionHelloAck)?;
                        transition!(WaitingForSelectedMessage {
                            timeout: wait.timeout,
                            stream: wait.stream,
                        })
                    }
                },
                _ => {
                    bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message")
                }
            }
        } else {
            match msg {
                Some(Protocol::StreamHello(stype)) => match stype {
                    None => {}
                    Some(StreamType::Relay(id)) => {}
                },
                _ => {
                    bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message")
                }
            }
        }
    }

    fn poll_waiting_for_selected_message<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForSelectedMessage<P>>,
    ) -> Poll<AfterWaitingForSelectedMessage<P>, Error> {
        loop {
            if let Err(_) = wait.timeout.poll() {
                // The other side will no send a message, when a connection is NOT selected.
                transition!(Finished(None));
            }

            match try_ready!(wait.stream.direct_poll()) {
                Some(Protocol::ConnectionSelected) => {
                    let mut wait = wait.take();
                    transition!(Finished(Some(wait.stream,)))
                }
                None => transition!(Finished(None)),
                _ => {}
            }
        }
    }
}
