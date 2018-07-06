use authenticator::Authenticator;
use config::Config;
use error::*;

use std::{
    cmp::min, io::{self, Read, Write}, net::SocketAddr,
};

use futures::{
    Async::{NotReady, Ready}, Future, Poll, Sink, StartSend, Stream as FStream,
};

use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use bytes::BytesMut;

use objekt;

mod quic;
mod udp;

trait StrategyTrait: FStream + LocalAddressInformation + NewConnection {}
impl<T: NewConnection + FStream + LocalAddressInformation> StrategyTrait for T {}

pub struct Strategy {
    inner: Box<StrategyTrait<Item = Connection, Error = Error>>,
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
    FStream + LocalAddressInformation + PeerAddressInformation + NewStream + GetConnectionId
{
}

impl<
        T: FStream + LocalAddressInformation + PeerAddressInformation + NewStream + GetConnectionId,
    > ConnectionTrait for T
{
}

pub type ConnectionId = u64;

pub struct Connection {
    inner: Box<ConnectionTrait<Item = Stream, Error = Error>>,
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
trait StreamTrait:
    FStream
    + LocalAddressInformation
    + PeerAddressInformation
    + Sink
    + NewStream
    + Send
    + GetConnectionId
{
}

impl<
        T: FStream
            + LocalAddressInformation
            + PeerAddressInformation
            + Sink
            + NewStream
            + Send
            + GetConnectionId,
    > StreamTrait for T
{
}

pub struct Stream {
    inner: Box<StreamTrait<Item = BytesMut, Error = Error, SinkItem = BytesMut, SinkError = Error>>,
    read_overflow: Option<BytesMut>,
}

impl Stream {
    fn new<
        C: StreamTrait<Item = BytesMut, Error = Error, SinkItem = BytesMut, SinkError = Error>
            + 'static,
    >(
        inner: C,
    ) -> Stream {
        let inner = Box::new(inner);
        Stream {
            inner,
            read_overflow: None,
        }
    }
}

pub trait LocalAddressInformation {
    fn local_addr(&self) -> SocketAddr;
}

pub trait PeerAddressInformation {
    fn peer_addr(&self) -> SocketAddr;
}

impl LocalAddressInformation for Stream {
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

impl PeerAddressInformation for Stream {
    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }
}

impl FStream for Stream {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

impl Sink for Stream {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }
}

impl NewStream for Stream {
    fn new_stream(&mut self) -> NewStreamFuture {
        self.inner.new_stream()
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.inner.get_new_stream_handle()
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        fn copy_data(mut src: BytesMut, dst: &mut [u8]) -> (usize, Option<BytesMut>) {
            let len = min(src.len(), dst.len());
            &dst[..len].copy_from_slice(&src[..len]);

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
        let res = self.inner.start_send(BytesMut::from(buf))?;

        if res.is_ready() {
            Ok(buf.len())
        } else {
            return Err(io::ErrorKind::WouldBlock.into());
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

impl GetConnectionId for Stream {
    fn connection_id(&self) -> ConnectionId {
        self.inner.connection_id()
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
    inner: Box<Future<Item = T, Error = Error>>,
}

impl<T> Future for NewTypeFuture<T> {
    type Item = T;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

impl<T> NewTypeFuture<T> {
    fn new<F: Future<Item = T, Error = Error> + 'static>(inner: F) -> NewTypeFuture<T> {
        let inner = Box::new(inner);
        NewTypeFuture { inner }
    }
}

pub type NewStreamFuture = NewTypeFuture<Stream>;
pub type NewConnectionFuture = NewTypeFuture<Connection>;

trait NewConnectionHandleTrait: NewConnection + objekt::Clone {}
impl<T: NewConnection + objekt::Clone> NewConnectionHandleTrait for T {}

pub struct NewConnectionHandle {
    inner: Box<NewConnectionHandleTrait>,
}

impl Clone for NewConnectionHandle {
    fn clone(&self) -> Self {
        NewConnectionHandle {
            inner: objekt::clone_box(&*self.inner),
        }
    }
}

impl NewConnectionHandle {
    fn new<T: NewConnection + Clone + 'static>(inner: T) -> NewConnectionHandle {
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
    inner: Box<NewStreamHandleTrait>,
}

impl Clone for NewStreamHandle {
    fn clone(&self) -> Self {
        NewStreamHandle {
            inner: objekt::clone_box(&*self.inner),
        }
    }
}

impl NewStreamHandle {
    fn new<T: NewStreamHandleTrait + 'static>(inner: T) -> NewStreamHandle {
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
    handle: Handle,
    config: &Config,
    authenticator: Authenticator,
) -> Result<Vec<Strategy>> {
    //Ok(vec![quic::init(handle, config, authenticator)?])
    Ok(vec![udp::init(handle, config, authenticator)?])
}

pub trait GetConnectionId {
    fn connection_id(&self) -> ConnectionId;
}
