use context::{PassStreamToContext, ResolvePeer};
use error::*;
use incoming_stream::IncomingStream;
use strategies::{self, NewConnection, NewStream};
use stream::{NewStreamFuture, NewStreamHandle, Stream};

use std::{net::SocketAddr, time::Duration};

use futures::{
    sync::oneshot, Async::{NotReady, Ready}, Future, Poll, Stream as FStream,
};

use serde::{Deserialize, Serialize};

use tokio_core::reactor::Handle;

pub type ConnectionId = u64;

#[derive(Clone)]
pub struct NewConnectionHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_con: strategies::NewConnectionHandle,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
}

impl<P, R> NewConnectionHandle<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        new_con: strategies::NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> NewConnectionHandle<P, R> {
        NewConnectionHandle {
            new_con,
            pass_stream_to_context,
            handle: handle.clone(),
            resolve_peer,
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture<P, R> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.clone(),
            self.pass_stream_to_context.clone(),
            self.resolve_peer.clone(),
            &self.handle,
        )
    }
}

pub struct NewConnectionFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    new_con_recv: strategies::NewConnectionFuture,
    pass_stream_to_context: PassStreamToContext<P, R>,
    new_con_handle: NewConnectionHandle<P, R>,
    resolve_peer: R,
    handle: Handle,
}

impl<P, R> NewConnectionFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        new_con_handle: NewConnectionHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
    ) -> NewConnectionFuture<P, R> {
        NewConnectionFuture {
            new_con_recv,
            new_con_handle,
            pass_stream_to_context,
            resolve_peer,
            handle: handle.clone(),
        }
    }
}

impl<P, R> Future for NewConnectionFuture<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    type Item = Connection<P, R>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.new_con_recv.poll().map(|r| {
            r.map(|v| {
                Connection::new(
                    v,
                    self.new_con_handle.clone(),
                    self.pass_stream_to_context.clone(),
                    self.resolve_peer.clone(),
                    &self.handle,
                    true,
                )
            })
        })
    }
}

enum ConnectionState {
    UnAuthenticated {
        auth_recv: oneshot::Receiver<()>,
        auth_send: Option<oneshot::Sender<()>>,
    },
    Authenticated,
}

pub struct Connection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    con: strategies::Connection,
    state: ConnectionState,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P, R>,
    resolve_peer: R,
    new_con_handle: NewConnectionHandle<P, R>,
    new_stream_handle: NewStreamHandle<P, R>,
}

impl<P, R> Connection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
{
    pub fn new(
        con: strategies::Connection,
        new_con_handle: NewConnectionHandle<P, R>,
        pass_stream_to_context: PassStreamToContext<P, R>,
        resolve_peer: R,
        handle: &Handle,
        is_authenticated: bool,
    ) -> Connection<P, R> {
        let (send, auth_recv) = oneshot::channel();
        let state = if is_authenticated {
            ConnectionState::Authenticated
        } else {
            ConnectionState::UnAuthenticated {
                auth_recv,
                auth_send: Some(send),
            }
        };

        let new_stream_handle = NewStreamHandle::new(
            con.get_new_stream_handle(),
            new_con_handle.clone(),
            pass_stream_to_context.clone(),
            resolve_peer.clone(),
            &handle,
        );

        Connection {
            con,
            state,
            handle: handle.clone(),
            pass_stream_to_context,
            resolve_peer,
            new_con_handle,
            new_stream_handle,
        }
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P, R> {
        NewStreamFuture::new(
            self.con.new_stream(),
            self.get_new_stream_handle(),
            self.get_new_con_handle(),
            self.pass_stream_to_context.clone(),
            self.resolve_peer.clone(),
            &self.handle,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle<P, R> {
        self.new_stream_handle.clone()
    }

    fn get_new_con_handle(&self) -> NewConnectionHandle<P, R> {
        self.new_con_handle.clone()
    }

    fn poll_impl(&mut self) -> Poll<Option<Stream<P, R>>, Error> {
        loop {
            let state = match self.state {
                ConnectionState::Authenticated => loop {
                    let stream = match try_ready!(self.con.poll()) {
                        Some(stream) => stream,
                        None => return Ok(Ready(None)),
                    };

                    return Ok(Ready(Some(Stream::new(
                        stream,
                        None,
                        &self.handle,
                        self.get_new_stream_handle(),
                        self.get_new_con_handle(),
                        self.pass_stream_to_context.clone(),
                        self.resolve_peer.clone(),
                    ))));
                },
                ConnectionState::UnAuthenticated {
                    ref mut auth_recv,
                    ref mut auth_send,
                } => {
                    loop {
                        let auth = match auth_recv.poll() {
                            Ok(Ready(())) => true,
                            _ => false,
                        };

                        if auth {
                            break;
                        } else {
                            let stream = match try_ready!(self.con.poll()) {
                                Some(stream) => stream,
                                None => return Ok(Ready(None)),
                            };

                            // Take `auth_send` and return the new `Stream`.
                            // If `auth_send` is None, we don't propagate any longer `Stream`s,
                            // because only one `Stream` is allowed for unauthorized `Connection`s.
                            if let Some(send) = auth_send.take() {
                                return Ok(Ready(Some(Stream::new(
                                    stream,
                                    Some(send),
                                    &self.handle,
                                    self.new_stream_handle.clone(),
                                    self.new_con_handle.clone(),
                                    self.pass_stream_to_context.clone(),
                                    self.resolve_peer.clone(),
                                ))));
                            }
                        }
                    }

                    ConnectionState::Authenticated
                }
            };

            self.state = state;
        }
    }
}

impl<P, R> Future for Connection<P, R>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
    R: ResolvePeer<P>,
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
                        self.resolve_peer.clone(),
                        &self.handle,
                    );
                    self.handle.spawn(
                        incoming_stream.map_err(|e| println!("IncomingStream error: {:?}", e)),
                    );
                }
            }
        }
    }
}
