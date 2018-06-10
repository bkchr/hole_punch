use context::{PassStreamToContext, ResolvePeer, ResolvePeerResult};
use error::*;
use protocol::{Protocol, StreamType};
use stream::Stream;
use timeout::Timeout;

use std::time::Duration;

use futures::{Async::Ready, Future, Poll, Sink, Stream as FStream};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

pub struct IncomingStream<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    stream: Option<Stream<P, R>>,
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
        timeout: Duration,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> IncomingStream<P, R> {
        IncomingStream {
            stream: Some(stream),
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

        match msg {
            Some(Protocol::Hello(stype)) => match stype {
                None => {
                    self.pass_stream_to_context
                        .pass_stream(self.stream.take().unwrap());
                }
                Some(StreamType::Relayed) => {
                    let mut stream = self.stream.take().unwrap();
                    stream.set_p2p(false);
                    self.pass_stream_to_context.pass_stream(stream);
                }
                Some(StreamType::Relay(peer)) => match self.resolve_peer.resolve_peer(&peer) {
                    ResolvePeerResult::FoundLocally(mut stream_handle) => {
                        let handle = self.handle.clone();
                        let stream = self.stream.take().unwrap();
                        self.handle.spawn(
                            stream_handle
                                .new_stream()
                                .and_then(move |mut stream2| {
                                    // Relay the initial `Hello`
                                    stream2.send_and_poll(StreamType::Relayed)?;

                                    let (sink0, fstream0) = stream.into_plain().split();
                                    let (sink1, fstream1) = stream2.into_plain().split();

                                    handle.spawn(
                                        sink0.send_all(fstream1).map_err(|_| ()).map(|_| ()),
                                    );
                                    handle.spawn(
                                        sink1.send_all(fstream0).map_err(|_| ()).map(|_| ()),
                                    );
                                    Ok(())
                                })
                                .map_err(|e| println!("{:?}", e)),
                        );
                    }
                    _ => {
                        self.stream
                            .take()
                            .unwrap()
                            .send_and_poll(Protocol::Error(format!(
                                "Could not find peer for relaying connection."
                            )))?;
                    }
                },
            },
            _ => bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message"),
        }

        Ok(Ready(()))
    }
}
