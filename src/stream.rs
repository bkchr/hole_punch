use crate::error::*;
use crate::protocol::StreamHello;
use crate::strategies::{self, NewStream};
use crate::PubKeyHash;

use std::{
    io::{self, Read, Write},
    net::SocketAddr,
    ops::Deref,
};

use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

use tokio_serde_json::{ReadJson, WriteJson};

use tokio::{
    codec::{Framed, LengthDelimitedCodec},
    io::{AsyncRead, AsyncWrite},
};

use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct NewStreamHandle {
    peer_identifier: PubKeyHash,
    local_peer_identifier: PubKeyHash,
    new_stream_handle: strategies::NewStreamHandle,
    proxy_stream: bool,
}

impl NewStreamHandle {
    pub fn new(
        peer_identifier: PubKeyHash,
        local_peer_identifier: PubKeyHash,
        new_stream_handle: strategies::NewStreamHandle,
    ) -> NewStreamHandle {
        NewStreamHandle {
            peer_identifier,
            local_peer_identifier,
            new_stream_handle,
            proxy_stream: false,
        }
    }

    pub(crate) fn set_proxy_stream(&mut self, proxy: bool, peer_identifier: PubKeyHash) {
        self.proxy_stream = proxy;
        self.peer_identifier = peer_identifier;
    }

    pub(crate) fn new_stream_with_hello(&mut self, stream_hello: StreamHello) -> NewStreamFuture {
        NewStreamFuture::new(
            self.peer_identifier.clone(),
            self.new_stream_handle.new_stream(),
            self.clone(),
            stream_hello,
        )
    }

    pub fn new_stream(&mut self) -> NewStreamFuture {
        let stream_hello = if self.proxy_stream {
            StreamHello::UserProxy(self.peer_identifier.clone())
        } else {
            StreamHello::User(self.local_peer_identifier.clone())
        };

        NewStreamFuture::new(
            self.peer_identifier.clone(),
            self.new_stream_handle.new_stream(),
            self.clone(),
            stream_hello,
        )
    }
}

pub struct NewStreamFuture {
    new_stream: strategies::NewStreamFuture,
    new_stream_handle: NewStreamHandle,
    stream_hello: StreamHello,
    peer_identifier: PubKeyHash,
}

impl NewStreamFuture {
    pub fn new(
        peer_identifier: PubKeyHash,
        new_stream: strategies::NewStreamFuture,
        new_stream_handle: NewStreamHandle,
        stream_hello: StreamHello,
    ) -> NewStreamFuture {
        NewStreamFuture {
            peer_identifier,
            new_stream,
            new_stream_handle,
            stream_hello,
        }
    }
}

impl Future for NewStreamFuture {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_stream.poll().map(|r| {
            r.map(|v| {
                let (peer_identifier, is_proxied) = match self.stream_hello {
                    StreamHello::UserProxy(ref peer)
                    | StreamHello::ProxyBuildConnectionToPeer(ref peer) => (peer.clone(), true),
                    _ => (self.peer_identifier.clone(), false),
                };

                let mut stream_hello: ProtocolStrategiesStream<StreamHello> = v.into();
                stream_hello
                    .start_send(self.stream_hello.clone())
                    .expect("start sending stream hello");
                stream_hello
                    .poll_complete()
                    .expect("poll complete stream hello");

                let stream = Stream::new(
                    stream_hello,
                    peer_identifier,
                    self.new_stream_handle.clone(),
                    is_proxied,
                );

                stream
            })
        })
    }
}

type StreamWithProtocol<S, P> = WriteJson<ReadJson<Framed<S, LengthDelimitedCodec>, P>, P>;

pub type ProtocolStream<P> = StreamWithProtocol<Stream, P>;

pub type ProtocolStrategiesStream<P> = StreamWithProtocol<strategies::Stream, P>;

impl<P> Into<ProtocolStream<P>> for Stream
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn into(self) -> ProtocolStream<P> {
        WriteJson::new(ReadJson::new(Framed::new(
            self,
            LengthDelimitedCodec::new(),
        )))
    }
}

impl<P> Into<ProtocolStrategiesStream<P>> for Stream
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn into(self) -> ProtocolStrategiesStream<P> {
        WriteJson::new(ReadJson::new(Framed::new(
            self.into(),
            LengthDelimitedCodec::new(),
        )))
    }
}

impl<P> Into<ProtocolStrategiesStream<P>> for strategies::Stream
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn into(self) -> ProtocolStrategiesStream<P> {
        WriteJson::new(ReadJson::new(Framed::new(
            self,
            LengthDelimitedCodec::new(),
        )))
    }
}

impl Into<strategies::Stream> for Stream {
    fn into(self) -> strategies::Stream {
        self.stream
    }
}

impl<P> From<ProtocolStream<P>> for Stream
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn from(stream: ProtocolStream<P>) -> Stream {
        let mut parts = stream.into_inner().into_inner().into_parts();
        parts.io.stream.reinsert_data(parts.read_buf);
        parts.io
    }
}

impl<P> From<ProtocolStrategiesStream<P>> for strategies::Stream
where
    P: 'static + Serialize + for<'de> Deserialize<'de>,
{
    fn from(stream: ProtocolStrategiesStream<P>) -> strategies::Stream {
        let mut parts = stream.into_inner().into_inner().into_parts();
        parts.io.reinsert_data(parts.read_buf);
        parts.io
    }
}

pub struct Stream {
    stream: strategies::Stream,
    new_stream_handle: NewStreamHandle,
    /// The identifier of the peer, if the stream is relayed it is the identifier of the peer the
    /// data is relayed too.
    peer_identifier: PubKeyHash,
    /// Is the stream proxied by another peer to the remote peer?
    is_proxy_stream: bool,
}

impl Stream {
    pub fn new<T>(
        stream: T,
        peer_identifier: PubKeyHash,
        mut new_stream_handle: NewStreamHandle,
        is_proxy_stream: bool,
    ) -> Stream
    where
        T: Into<strategies::Stream>,
    {
        new_stream_handle.set_proxy_stream(is_proxy_stream, peer_identifier.clone());

        Stream {
            stream: stream.into(),
            peer_identifier,
            new_stream_handle,
            is_proxy_stream,
        }
    }

    pub fn is_p2p(&self) -> bool {
        !self.is_proxy_stream
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }

    pub fn new_stream_handle(&self) -> &NewStreamHandle {
        &self.new_stream_handle
    }

    pub fn peer_identifier(&self) -> &PubKeyHash {
        &self.peer_identifier
    }

    pub fn set_send_channel_size(&mut self, size: usize) {
        self.stream.set_send_channel_size(size);
    }
}

impl FStream for Stream {
    type Item = <<strategies::Stream as Deref>::Target as FStream>::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.stream.poll().map_err(|e| e.into())
    }
}

impl Sink for Stream {
    type SinkItem = <<strategies::Stream as Deref>::Target as Sink>::SinkItem;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.stream.start_send(item).map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete().map_err(|e| e.into())
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.stream)
    }
}

impl AsyncRead for Stream {}

impl AsyncWrite for Stream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.stream.shutdown()
    }
}
