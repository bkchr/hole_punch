use build_connection_to_peer::{prepare_streams_for_building, BuildConnectionToPeerRemote};
use connection::NewConnectionHandle;
use context::PassStreamToContext;
use error::*;
use protocol::StreamHello;
use registry::Registry;
use remote_registry;
use strategies;
use stream::{NewStreamHandle, ProtocolStream};
use timeout::Timeout;
use PubKeyHash;

use std::time::Duration;

use futures::{Async::Ready, Future, Poll, Sink, Stream as FStream};

use bytes::BytesMut;

use tokio;

pub struct IncomingStream {
    stream: Option<ProtocolStream<StreamHello>>,
    timeout: Timeout,
    pass_stream_to_context: PassStreamToContext,
    registry: Registry,
    peer_identifier: PubKeyHash,
    new_stream_handle: NewStreamHandle,
    new_con_handle: NewConnectionHandle,
}

impl IncomingStream {
    pub fn new(
        stream: strategies::Stream,
        timeout: Duration,
        pass_stream_to_context: PassStreamToContext,
        registry: Registry,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
        new_con_handle: NewConnectionHandle,
    ) -> IncomingStream {
        IncomingStream {
            stream: Some(stream.into()),
            timeout: Timeout::new(timeout),
            pass_stream_to_context,
            registry,
            peer_identifier,
            new_stream_handle,
            new_con_handle,
        }
    }
}

impl Future for IncomingStream {
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
                .poll()
        );

        match msg {
            Some(StreamHello::User(peer)) => {
                let mut stream = self.stream.take().unwrap();

                let is_proxy_stream = peer != self.peer_identifier;

                self.pass_stream_to_context.pass_stream(
                    stream.into(),
                    peer,
                    self.new_stream_handle.clone(),
                    is_proxy_stream,
                );
            }
            Some(StreamHello::UserProxy(peer)) => {
                handle_proxy_stream(
                    peer,
                    self.stream.take().unwrap().into(),
                    &self.registry,
                    StreamHello::User(self.peer_identifier.clone()),
                    |stream0, stream1| Ok((stream0, stream1)),
                );
            }
            Some(StreamHello::Registry) => {
                tokio::spawn(
                    remote_registry::IncomingStream::new(
                        self.stream.take().unwrap(),
                        self.registry.clone(),
                    ).map_err(|e| error!("IncomingStream error: {:?}", e)),
                );
            }
            Some(StreamHello::BuildConnectionToPeer(peer)) => {
                tokio::spawn(
                    BuildConnectionToPeerRemote::new(
                        self.stream.take().unwrap().into(),
                        peer,
                        self.new_stream_handle.clone(),
                        self.pass_stream_to_context.clone(),
                        self.new_con_handle.clone(),
                    ).map_err(|e| error!("BuildConnectionToPeerRemote error: {:?}", e)),
                );
            }
            Some(StreamHello::ProxyBuildConnectionToPeer(peer)) => {
                handle_proxy_stream(
                    peer,
                    self.stream.take().unwrap().into(),
                    &self.registry,
                    StreamHello::BuildConnectionToPeer(self.peer_identifier.clone()),
                    |stream0, stream1| prepare_streams_for_building(stream0, stream1),
                );
            }
            None => {}
        }

        Ok(Ready(()))
    }
}

fn handle_proxy_stream<F>(
    target_peer_identifier: PubKeyHash,
    stream: strategies::Stream,
    registry: &Registry,
    hello_msg: StreamHello,
    prepare_streams: F,
) where
    F: 'static
        + Fn(strategies::Stream, strategies::Stream)
            -> Result<(strategies::Stream, strategies::Stream)>
        + Send,
{
    if let Some(mut peer_new_stream_handle) = registry.peer(&target_peer_identifier) {
        tokio::spawn(
            peer_new_stream_handle
                .new_stream_with_hello(hello_msg)
                .and_then(move |stream2| {
                    let (stream, stream2) = match prepare_streams(stream, stream2.into()) {
                        Ok(res) => res,
                        // TODO: better handling?
                        Err(e) => bail!("error preparing stream: {:?}", e),
                    };

                    let (sink0, fstream0) = stream.split();
                    let (sink1, fstream1) = stream2.split();

                    tokio::spawn(
                        sink0
                            .send_all(fstream1.map(BytesMut::freeze))
                            .select(sink1.send_all(fstream0.map(BytesMut::freeze)))
                            .map_err(|_| ())
                            .map(|_| ()),
                    );
                    Ok(())
                }).map_err(|e| error!("{:?}", e)),
        );
    } else {
        // TODO: We should notify the other side and not just drop.
        info!("Could not find requested peer in registry.");
    }
}
