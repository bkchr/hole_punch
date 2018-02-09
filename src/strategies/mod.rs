use errors::*;
use config::Config;

use std::net::SocketAddr;
use std::io::{self, Read, Write};
use std::cmp::min;

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream};

use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use bytes::BytesMut;

mod udp;

trait StrategyTrait: Stream<Item = Connection, Error = Error> + NewConnection {}
impl<T: NewConnection + Future<Item = Connection, Error = Error>> StrategyTrait for T {}

pub struct Strategy {
    inner: Box<StrategyTrait<Item = Connection, Error = Error>>,
}

impl Strategy {
    fn new<S: StrategyTrait<Item = Connection, Error = Error>>(inner: S) -> Strategy {
        let inner = Box::new(inner);
        Strategy { inner }
    }
}

impl Stream for Strategy {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

/// The super `Connection` trait. We need this hack, to store the `inner` of the connection
/// in a `Box`.
trait ConnectionTrait
    : Stream<Item = BytesMut, Error = Error>
    + AddressInformation
    + Sink<SinkItem = BytesMut, SinkError = Error>
    + NewSession {
}

impl<
    T: Stream<Item = BytesMut, Error = Error>
        + AddressInformation
        + Sink<SinkItem = BytesMut, SinkError = Error>
        + NewSession,
> ConnectionTrait for T
{
}

pub struct Connection {
    inner: Box<
        ConnectionTrait<Item = BytesMut, Error = Error, SinkItem = BytesMut, SinkError = Error>,
    >,
    read_overflow: Option<BytesMut>,
}

impl Connection {
    fn new<
        C: ConnectionTrait<Item = BytesMut, Error = Error, SinkItem = BytesMut, SinkError = Error>,
    >(
        inner: C,
    ) -> Connection {
        let inner = Box::new(inner);
        Connection { inner }
    }
}

pub trait AddressInformation {
    fn local_addr(&self) -> SocketAddr;
    fn peer_addr(&self) -> SocketAddr;
}

impl AddressInformation for Connection {
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }
}

impl Stream for Connection {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

impl Sink for Connection {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }
}

impl NewSession for Connection {
    fn new_session(&mut self) -> NewConnectionFuture {
        self.inner.new_session()
    }
}

impl Read for Connection {
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

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // TODO find a way to check if the Sink can send, before we try to write!
        let res = self.inner.start_send(BytesMut::from(buf))?;

        if res.is_ready() {
            Ok(buf.len())
        } else {
            io::ErrorKind::WouldBlock.into()?
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.poll_complete()?;
        Ok(())
    }
}

impl AsyncRead for Connection {}

impl AsyncWrite for Connection {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.inner.close()?;
        Ok(Ready(()))
    }
}

pub trait NewSession {
    fn new_session(&mut self) -> NewConnectionFuture;
}

pub trait NewConnection {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture;
    fn get_new_connection_handle(&mut self) -> NewConnectionHandle;
}

pub struct NewConnectionFuture {
    inner: Box<Future<Item = Connection, Error = Error>>,
}

impl Future for NewConnectionFuture {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

impl NewConnectionFuture {
    fn new<F: Future<Item = Connection, Error = Error> + 'static>(inner: F) -> NewConnectionFuture {
        let inner = Box::new(inner);
        NewConnectionFuture { inner }
    }
}

#[derive(Clone)]
pub struct NewConnectionHandle {
    inner: Box<NewConnection>,
}

impl Clone for Box<NewConnection> {
    fn clone(&self) -> Box<NewConnection> {
        Box::new((&self).clone())
    }
}

impl NewConnectionHandle {
    fn new<H: NewConnection>(inner: H) -> NewConnectionHandle {
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

pub fn init(handle: Handle, config: &Config) -> Vec<Strategy> {
    vec![udp::init(handle, config)]
}
