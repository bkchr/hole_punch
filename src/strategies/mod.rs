use crate::authenticator::Authenticator;
use crate::config::Config;
use crate::context::SendFuture;
use crate::error::*;

use std::{
    cmp::min,
    io::{self, Read, Write},
    net::SocketAddr,
    ops::{Deref, DerefMut},
};

use futures::{
    stream::{SplitSink, SplitStream},
    Async::{NotReady, Ready},
    Future, Poll, Sink, Stream as FStream,
};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    runtime::TaskExecutor,
};

use bytes::{Bytes, BytesMut};

use objekt;

mod quic;

trait StrategyTrait: FStream + LocalAddressInformation + NewConnection + Send {}
impl<T: NewConnection + FStream + LocalAddressInformation + Send> StrategyTrait for T {}

pub struct Strategy {
    inner: Box<dyn StrategyTrait<Item = Connection, Error = Error>>,
}

impl Strategy {
    fn new<S: StrategyTrait<Item = Connection, Error = Error> + 'static>(inner: S) -> Strategy {
        let inner = Box::new(inner);
        Strategy { inner }
    }
}

impl FStream for Strategy {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

impl NewConnection for Strategy {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        self.inner.new_connection(addr)
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        self.inner.get_new_connection_handle()
    }
}

impl LocalAddressInformation for Strategy {
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

/// The super `Connection` trait. We need this hack, to store the `inner` of the connection
/// in a `Box`.
trait ConnectionTrait:
    FStream + LocalAddressInformation + PeerAddressInformation + NewStream + GetConnectionId + Send
{
}

impl<
        T: FStream
            + LocalAddressInformation
            + PeerAddressInformation
            + NewStream
            + GetConnectionId
            + Send,
    > ConnectionTrait for T
{
}

pub type ConnectionId = u64;

pub struct Connection {
    inner: Box<dyn ConnectionTrait<Item = Stream, Error = Error>>,
}

impl Connection {
    fn new<C: ConnectionTrait<Item = Stream, Error = Error> + 'static>(inner: C) -> Connection {
        let inner = Box::new(inner);
        Connection { inner }
    }
}

impl LocalAddressInformation for Connection {
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

impl PeerAddressInformation for Connection {
    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }
}

impl NewStream for Connection {
    fn new_stream(&mut self) -> NewStreamFuture {
        self.inner.new_stream()
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.inner.get_new_stream_handle()
    }
}

impl FStream for Connection {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

impl GetConnectionId for Connection {
    fn connection_id(&self) -> ConnectionId {
        self.inner.connection_id()
    }
}

/// The super `Stream` trait. We need this hack, to store the `inner` of the stream in a `Box`.
pub trait StreamTrait:
    FStream
    + LocalAddressInformation
    + PeerAddressInformation
    + Sink
    + NewStream
    + Send
    + GetConnectionId
    + SetSendChannelSize
{
}

impl<
        T: FStream
            + LocalAddressInformation
            + PeerAddressInformation
            + Sink
            + NewStream
            + Send
            + GetConnectionId
            + SetSendChannelSize,
    > StreamTrait for T
{
}

type StreamInner =
    Box<dyn StreamTrait<Item = BytesMut, Error = Error, SinkItem = Bytes, SinkError = Error>>;

pub struct Stream {
    inner: StreamInner,
    read_overflow: Option<BytesMut>,
}

impl Stream {
    fn new<
        C: StreamTrait<Item = BytesMut, Error = Error, SinkItem = Bytes, SinkError = Error> + 'static,
    >(
        inner: C,
    ) -> Stream {
        let inner = Box::new(inner);
        Stream {
            inner,
            read_overflow: None,
        }
    }

    pub fn split(self) -> (SplitSink<StreamInner>, SplitStream<StreamInner>) {
        self.inner.split()
    }

    /// If we switch the protocol and the old protocol already read more data than it required,
    /// this function can be used to reinsert the data that it is available for the next protocol.
    pub fn reinsert_data(&mut self, mut data: BytesMut) {
        if data.is_empty() {
            return;
        }

        match self.read_overflow.take() {
            Some(overflow) => {
                data.extend_from_slice(&overflow);
                self.read_overflow = Some(data);
            }
            None => {
                self.read_overflow = Some(data);
            }
        }
    }
}

impl FStream for Stream {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.read_overflow.take() {
            Some(buf) => Ok(Ready(Some(buf))),
            None => self.inner.poll(),
        }
    }
}

impl Deref for Stream {
    type Target = StreamInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut StreamInner {
        &mut self.inner
    }
}

pub trait LocalAddressInformation {
    fn local_addr(&self) -> SocketAddr;
}

pub trait PeerAddressInformation {
    fn peer_addr(&self) -> SocketAddr;
}

pub trait SetSendChannelSize {
    fn set_send_channel_size(&mut self, size: usize);
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        fn copy_data(mut src: BytesMut, dst: &mut [u8]) -> (usize, Option<BytesMut>) {
            let len = min(src.len(), dst.len());
            dst[..len].copy_from_slice(&src[..len]);

            if src.len() > len {
                src.advance(len);
                (len, Some(src))
            } else {
                (len, None)
            }
        }

        if let Some(data) = self.read_overflow.take() {
            let (len, overflow) = copy_data(data, buf);
            self.read_overflow = overflow;

            Ok(len)
        } else {
            let res = self.inner.poll()?;

            match res {
                NotReady => Err(io::ErrorKind::WouldBlock.into()),
                Ready(Some(data)) => {
                    let (len, overflow) = copy_data(data, buf);
                    self.read_overflow = overflow;

                    Ok(len)
                }
                Ready(None) => Ok(0),
            }
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // TODO find a way to check if the Sink can send, before we try to write!
        let res = self.inner.start_send(Bytes::from(buf))?;

        if res.is_ready() {
            Ok(buf.len())
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.poll_complete()?;
        Ok(())
    }
}

impl AsyncRead for Stream {}

impl AsyncWrite for Stream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.inner.close()?;
        Ok(Ready(()))
    }
}

pub trait NewStream {
    fn new_stream(&mut self) -> NewStreamFuture;
    fn get_new_stream_handle(&self) -> NewStreamHandle;
}

pub trait NewConnection {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture;
    fn get_new_connection_handle(&self) -> NewConnectionHandle;
}

pub struct NewTypeFuture<T> {
    inner: Box<dyn SendFuture<Item = T, Error = Error>>,
}

impl<T> Future for NewTypeFuture<T> {
    type Item = T;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

impl<T: Send> NewTypeFuture<T> {
    fn new<F: SendFuture<Item = T, Error = Error> + 'static>(inner: F) -> NewTypeFuture<T> {
        let inner = Box::new(inner);
        NewTypeFuture { inner }
    }
}

pub type NewStreamFuture = NewTypeFuture<Stream>;
pub type NewConnectionFuture = NewTypeFuture<Connection>;

trait NewConnectionHandleTrait: NewConnection + objekt::Clone + Send {}
impl<T: NewConnection + objekt::Clone + Send> NewConnectionHandleTrait for T {}

pub struct NewConnectionHandle {
    inner: Box<dyn NewConnectionHandleTrait>,
}

impl Clone for NewConnectionHandle {
    fn clone(&self) -> Self {
        NewConnectionHandle {
            inner: objekt::clone_box(&*self.inner),
        }
    }
}

impl NewConnectionHandle {
    fn new<T: NewConnection + Clone + 'static + Send>(inner: T) -> NewConnectionHandle {
        let inner = Box::new(inner);
        NewConnectionHandle { inner }
    }
}

impl NewConnection for NewConnectionHandle {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        self.inner.new_connection(addr)
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        self.clone()
    }
}

trait NewStreamHandleTrait: NewStream + objekt::Clone + Send {}
impl<T: NewStream + objekt::Clone + Send> NewStreamHandleTrait for T {}

pub struct NewStreamHandle {
    inner: Box<dyn NewStreamHandleTrait>,
}

impl Clone for NewStreamHandle {
    fn clone(&self) -> Self {
        NewStreamHandle {
            inner: objekt::clone_box(&*self.inner),
        }
    }
}

impl NewStreamHandle {
    fn new<T: NewStream + Clone + 'static + Send>(inner: T) -> NewStreamHandle {
        let inner = Box::new(inner);
        NewStreamHandle { inner }
    }
}

impl NewStream for NewStreamHandle {
    fn new_stream(&mut self) -> NewStreamFuture {
        self.inner.new_stream()
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.clone()
    }
}

pub fn init(
    handle: TaskExecutor,
    config: &Config,
    authenticator: Authenticator,
) -> Result<Vec<Strategy>> {
    Ok(vec![quic::init(handle, config, authenticator)?])
}

pub trait GetConnectionId {
    fn connection_id(&self) -> ConnectionId;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StartSend;
    use std::collections::VecDeque;

    struct StreamMock {
        data: VecDeque<BytesMut>,
    }

    impl FStream for StreamMock {
        type Item = BytesMut;
        type Error = Error;

        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            Ok(Ready(self.data.pop_front()))
        }
    }

    impl Sink for StreamMock {
        type SinkItem = Bytes;
        type SinkError = Error;

        fn start_send(&mut self, _: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
            unimplemented!()
        }

        fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
            unimplemented!()
        }
    }

    impl LocalAddressInformation for StreamMock {
        fn local_addr(&self) -> SocketAddr {
            unimplemented!()
        }
    }
    impl PeerAddressInformation for StreamMock {
        fn peer_addr(&self) -> SocketAddr {
            unimplemented!()
        }
    }

    impl NewStream for StreamMock {
        fn new_stream(&mut self) -> NewStreamFuture {
            unimplemented!()
        }

        fn get_new_stream_handle(&self) -> NewStreamHandle {
            unimplemented!()
        }
    }

    impl GetConnectionId for StreamMock {
        fn connection_id(&self) -> ConnectionId {
            unimplemented!()
        }
    }

    impl SetSendChannelSize for StreamMock {
        fn set_send_channel_size(&mut self, _: usize) {
            unimplemented!()
        }
    }

    #[test]
    fn read_and_poll_stream_does_not_loose_data() {
        let stream = StreamMock {
            data: vec![BytesMut::from(vec![1; 100])].into(),
        };
        let mut stream = Stream::new(stream);

        let mut buf = vec![0; 50];
        assert_eq!(stream.poll_read(&mut buf).unwrap(), Ready(50));

        let buf = stream.poll().unwrap();
        assert_eq!(buf.map(|b| b.map(|b| b.len())), Ready(Some(50)));
    }

    #[test]
    fn read_and_poll_stream_does_not_loose_data_with_reinsert() {
        let input_data = (0..100).collect::<Vec<u8>>();
        let stream = StreamMock {
            data: vec![BytesMut::from(input_data.clone())].into(),
        };
        let mut stream = Stream::new(stream);

        let mut buf = vec![0; 50];
        assert_eq!(stream.poll_read(&mut buf).unwrap(), Ready(50));

        stream.reinsert_data(BytesMut::from(buf));
        let buf = stream.poll().unwrap();
        assert_eq!(buf.map(|b| b.map(|b| b.to_vec())), Ready(Some(input_data)));
    }
}
