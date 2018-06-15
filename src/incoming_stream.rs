use context::PassStreamToContext;
use error::*;
use protocol::{Protocol, StreamPurpose};
use registry::Registry;
use strategies;
use stream::{Stream, StreamJsonWrapper};
use timeout::Timeout;

use std::time::Duration;

use futures::{Async::Ready, Future, Poll, Sink, Stream as FStream};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use tokio_io::codec::length_delimited;

pub struct IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    stream: Option<StreamJsonWrapper<P>>,
    timeout: Timeout,
    pass_stream_to_context: PassStreamToContext<P>,
    registry: Registry<P>,
    handle: Handle,
}

impl<P> IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        stream: strategies::Stream,
        timeout: Duration,
        pass_stream_to_context: PassStreamToContext<P>,
        registry: Registry<P>,
        handle: Handle,
    ) -> IncomingStream<P> {
        IncomingStream {
            stream: Some(WriteJson::new(ReadJson::new(
                length_delimited::Framed::new(stream),
            ))),
            timeout: Timeout::new(timeout, handle),
            pass_stream_to_context,
            registry,
            handle,
        }
    }
}

impl<P> Future for IncomingStream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
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
                .poll_inner()
        );

        match msg {
            Some(Protocol::Hello(stype)) => match stype {
                None => {
                    self.pass_stream_to_context
                        .pass_stream(self.stream.take().unwrap());
                }
                Some(StreamPurpose::Relayed(remote)) => {
                    let mut stream = self.stream.take().unwrap();
                    stream.set_relayed(remote);
                    self.pass_stream_to_context.pass_stream(stream);
                }
                Some(StreamPurpose::Relay(origin, peer)) => {
                    match self.resolve_peer.resolve_peer(&peer) {
                        ResolvePeerResult::FoundLocally(mut stream_handle) => {
                            let handle = self.handle.clone();
                            let stream = self.stream.take().unwrap();
                            self.handle.spawn(
                                stream_handle
                                    .new_stream_with_hello(StreamPurpose::Relayed(origin).into())
                                    .and_then(move |stream2| {
                                        let (sink0, fstream0) = stream.into_inner().split();
                                        let (sink1, fstream1) = stream2.into_inner().split();

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
                    }
                }
            },
            _ => bail!("unexpected message({:?}) at IncomingStream::waiting_for_initial_message"),
        }

        Ok(Ready(()))
    }
}
