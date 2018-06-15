use authenticator::Authenticator;
use context::PassStreamToContext;
use error::*;
use incoming_stream::IncomingStream;
use protocol::Protocol;
use registry::Registry;
use strategies::{self, NewConnection, NewStream};
use stream::{NewStreamFuture, NewStreamHandle, Stream};
use PubKeyHash;

use std::{net::SocketAddr, time::Duration};

use futures::{
    sync::oneshot, Async::{NotReady, Ready}, Future, Poll, Stream as FStream,
};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

pub type ConnectionId = u64;

#[derive(Clone)]
pub struct NewConnectionHandle<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    new_con: strategies::NewConnectionHandle,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P>,
    registry: Registry<P>,
    authenticator: Authenticator,
}

impl<P> NewConnectionHandle<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        new_con: strategies::NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext<P>,
        registry: Registry<P>,
        handle: Handle,
        authenticator: Authenticator,
    ) -> NewConnectionHandle<P> {
        NewConnectionHandle {
            new_con,
            pass_stream_to_context,
            handle: handle.clone(),
            registry,
            authenticator,
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture<P> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.clone(),
            self.pass_stream_to_context.clone(),
            self.registry.clone(),
            self.handle.clone(),
            self.authenticator.clone(),
        )
    }
}

pub struct NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    new_con_recv: strategies::NewConnectionFuture,
    pass_stream_to_context: PassStreamToContext<P>,
    new_con_handle: NewConnectionHandle<P>,
    handle: Handle,
    registry: Registry<P>,
    authenticator: Authenticator,
}

impl<P> NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        new_con_handle: NewConnectionHandle<P>,
        pass_stream_to_context: PassStreamToContext<P>,
        registry: Registry<P>,
        handle: Handle,
        authenticator: Authenticator,
    ) -> NewConnectionFuture<P> {
        NewConnectionFuture {
            new_con_recv,
            new_con_handle,
            pass_stream_to_context,
            handle: handle.clone(),
            registry,
            authenticator,
        }
    }
}

impl<P> Future for NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Connection<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_con_recv.poll().map(|r| {
            r.map(|v| {
                Connection::new(
                    v,
                    self.new_con_handle.clone(),
                    self.pass_stream_to_context.clone(),
                    self.registry.clone(),
                    self.handle.clone(),
                    self.authenticator.clone(),
                )
            })
        })
    }
}

pub struct Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    con: strategies::Connection,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P>,
    new_con_handle: NewConnectionHandle<P>,
    new_stream_handle: NewStreamHandle<P>,
    registry: Registry<P>,
    /// The identifier of the peer this Connection is connected to.
    peer_identifier: PubKeyHash,
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        con: strategies::Connection,
        new_con_handle: NewConnectionHandle<P>,
        pass_stream_to_context: PassStreamToContext<P>,
        registry: Registry<P>,
        handle: Handle,
        authenticator: Authenticator,
    ) -> Connection<P> {
        let (send, auth_recv) = oneshot::channel();

        let new_stream_handle = NewStreamHandle::new(
            con.get_new_stream_handle(),
            new_con_handle.clone(),
            registry.clone(),
            handle.clone(),
        );

        let peer_identifier = authenticator
            .incoming_con_pub_key(&con)
            .expect("Could not find public key for connection!");

        Connection {
            con,
            handle,
            pass_stream_to_context,
            new_con_handle,
            new_stream_handle,
            registry,
            peer_identifier,
        }
    }

    pub fn new_stream_with_hello(&mut self, hello_msg: Protocol<P>) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.peer_identifier.clone(),
            self.con.new_stream(),
            self.get_new_stream_handle(),
            self.get_new_con_handle(),
            self.registry.clone(),
            hello_msg,
            self.handle.clone(),
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle<P> {
        self.new_stream_handle.clone()
    }

    fn get_new_con_handle(&self) -> NewConnectionHandle<P> {
        self.new_con_handle.clone()
    }

    fn poll_impl(&mut self) -> Poll<Option<Stream<P>>, Error> {
        let stream = match try_ready!(self.con.poll()) {
            Some(stream) => stream,
            None => return Ok(Ready(None)),
        };

        return Ok(Ready(Some(Stream::new(
            stream,
            self.peer_identifier.clone(),
            self.handle.clone(),
            self.get_new_stream_handle(),
            self.get_new_con_handle(),
            self.registry.clone(),
        ))));
    }
}

impl<P> Future for Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.poll_impl() {
                Ok(NotReady) => return Ok(NotReady),
                Err(e) => {
                    println!("Connection: {:?}", e);
                    return Ok(Ready(()));
                }
                Ok(Ready(None)) => return Ok(Ready(())),
                Ok(Ready(Some(stream))) => {
                    let incoming_stream = IncomingStream::new(
                        stream,
                        Duration::from_secs(10),
                        self.pass_stream_to_context.clone(),
                        self.registry.clone(),
                        self.handle.clone(),
                    );
                    self.handle.spawn(
                        incoming_stream.map_err(|e| println!("IncomingStream error: {:?}", e)),
                    );
                }
            }
        }
    }
}
