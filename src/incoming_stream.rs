use context::{PassStreamToContext, ResolvePeer, ResolvePeerResult};
use error::*;
use protocol::{Protocol, StreamType};
use stream::Stream;
use timeout::Timeout;

use std::time::Duration;

use futures::{Async::Ready, Future, Poll, Stream as FStream};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

pub struct IncomingStream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    stream: Option<Stream<P, R>>,
    // Is this the first stream of the `Connection`?
    first_stream: bool,
    timeout: Timeout,
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
    handle: Handle,
}

impl<P, R> IncomingStream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        stream: Stream<P, R>,
        first_stream: bool,
        timeout: Duration,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> IncomingStream<P, R> {
        IncomingStream {
            stream: Some(stream),
            first_stream,
            timeout: Timeout::new(timeout, handle),
            pass_stream_to_context,
            resolve_peer,
            handle: handle.clone(),
        }
    }
}

impl<P, R> Future for IncomingStream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Err(_) = self.timeout.poll() {
            bail!("timeout at IncomingStream::poll()");
        }

        let msg = try_ready!(
            self.stream
                .as_mut()
                .expect("Can not be polled twice!")
                .direct_poll()
        );

        if self.first_stream {
            match msg {
                Some(Protocol::ConnectionHello) => {
                    self.pass_stream_to_context
                        .pass_stream(self.stream.take().unwrap());
                }
                _ => {
                    bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message")
                }
            }
        } else {
            match msg {
                Some(Protocol::StreamHello(stype)) => match stype {
                    None => {
                        self.pass_stream_to_context
                            .pass_stream(self.stream.take().unwrap());
                    }
                    Some(StreamType::Relay(peer)) => {
                        match self.resolve_peer.resolve_peer(&peer) {
                            ResolvePeerResult::Found(handle) => {}
                            _ => {
                                self.stream.take().unwrap().send_and_poll(Protocol::Error(
                                    format!("Could not find peer for relaying connection."),
                                ));
                            }
                        }
                    }
                },
                _ => {
                    bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message")
                }
            }
        }

        Ok(Ready(()))
    }
}
