use authenticator::Authenticator;
use context::PassStreamToContext;
use error::*;
use incoming_stream::IncomingStream;
use protocol::StreamHello;
use registry::{RegistrationToken, Registry};
use strategies::{self, NewConnection, NewStream};
use stream::{NewStreamFuture, NewStreamHandle};
use PubKeyHash;

use std::{net::SocketAddr, time::Duration};

use futures::{
    Async::{NotReady, Ready},
    Future, Poll, Stream as FStream,
};

use tokio_core::reactor::Handle;

#[derive(Clone)]
pub struct NewConnectionHandle {
    new_con: strategies::NewConnectionHandle,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext,
    registry: Registry,
    authenticator: Authenticator,
    local_peer_identifier: PubKeyHash,
}

impl NewConnectionHandle {
    pub fn new(
        local_peer_identifier: PubKeyHash,
        new_con: strategies::NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext,
        registry: Registry,
        handle: Handle,
        authenticator: Authenticator,
    ) -> NewConnectionHandle {
        NewConnectionHandle {
            new_con,
            pass_stream_to_context,
            handle,
            registry,
            authenticator,
            local_peer_identifier,
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.local_peer_identifier.clone(),
            self.clone(),
            self.pass_stream_to_context.clone(),
            self.registry.clone(),
            self.handle.clone(),
            self.authenticator.clone(),
        )
    }
}

pub struct NewConnectionFuture {
    new_con_recv: strategies::NewConnectionFuture,
    pass_stream_to_context: PassStreamToContext,
    new_con_handle: NewConnectionHandle,
    handle: Handle,
    registry: Registry,
    authenticator: Authenticator,
    local_peer_identifier: PubKeyHash,
}

impl NewConnectionFuture {
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        local_peer_identifier: PubKeyHash,
        new_con_handle: NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext,
        registry: Registry,
        handle: Handle,
        authenticator: Authenticator,
    ) -> NewConnectionFuture {
        NewConnectionFuture {
            new_con_recv,
            new_con_handle,
            pass_stream_to_context,
            handle,
            registry,
            authenticator,
            local_peer_identifier,
        }
    }
}

impl Future for NewConnectionFuture {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_con_recv.poll().and_then(|r| match r {
            Ready(v) => Ok(Ready(Connection::new(
                v,
                self.local_peer_identifier.clone(),
                self.new_con_handle.clone(),
                self.pass_stream_to_context.clone(),
                self.registry.clone(),
                self.handle.clone(),
                self.authenticator.clone(),
            )?)),
            NotReady => Ok(NotReady),
        })
    }
}

pub struct Connection {
    con: strategies::Connection,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext,
    new_con_handle: NewConnectionHandle,
    new_stream_handle: NewStreamHandle,
    registry: Registry,
    /// The identifier of the peer this Connection is connected to.
    peer_identifier: PubKeyHash,
    registration_token: RegistrationToken,
}

impl Connection {
    pub fn new(
        con: strategies::Connection,
        local_peer_identifier: PubKeyHash,
        new_con_handle: NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext,
        registry: Registry,
        handle: Handle,
        mut authenticator: Authenticator,
    ) -> Result<Connection> {
        let peer_identifier = match authenticator.incoming_con_pub_key(&con) {
            Some(key) => key,
            None => bail!("Could not find public key for connection!"),
        };

        let new_stream_handle = NewStreamHandle::new(
            peer_identifier.clone(),
            local_peer_identifier,
            con.get_new_stream_handle(),
        );

        let registration_token =
            registry.register_peer(peer_identifier.clone(), new_stream_handle.clone());

        Ok(Connection {
            con,
            handle,
            pass_stream_to_context,
            new_con_handle,
            new_stream_handle,
            registry,
            peer_identifier,
            registration_token,
        })
    }

    pub fn new_stream_with_hello(&mut self, stream_hello: StreamHello) -> NewStreamFuture {
        NewStreamFuture::new(
            self.peer_identifier.clone(),
            self.con.new_stream(),
            self.get_new_stream_handle(),
            stream_hello,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        self.new_stream_handle.clone()
    }

    fn poll_impl(&mut self) -> Poll<Option<strategies::Stream>, Error> {
        let stream = match try_ready!(self.con.poll()) {
            Some(stream) => stream,
            None => return Ok(Ready(None)),
        };

        return Ok(Ready(Some(stream)));
    }
}

impl Future for Connection {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.poll_impl() {
                Ok(NotReady) => return Ok(NotReady),
                Err(e) => {
                    self.registry
                        .unregister_peer(self.peer_identifier.clone(), self.registration_token);
                    println!("Connection: {:?}", e);
                    return Ok(Ready(()));
                }
                Ok(Ready(None)) => {
                    self.registry
                        .unregister_peer(self.peer_identifier.clone(), self.registration_token);
                    return Ok(Ready(()));
                }
                Ok(Ready(Some(stream))) => {
                    let incoming_stream = IncomingStream::new(
                        stream,
                        Duration::from_secs(10),
                        self.pass_stream_to_context.clone(),
                        self.registry.clone(),
                        self.peer_identifier.clone(),
                        self.new_stream_handle.clone(),
                        self.new_con_handle.clone(),
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
