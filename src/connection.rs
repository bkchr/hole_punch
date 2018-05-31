use error::*;
use strategies::{self, NewConnection, NewStream};
use stream::{NewStreamFuture, NewStreamHandle, Stream, StreamHandle};

use std::marker::PhantomData;
use std::net::SocketAddr;

use futures::sync::{mpsc, oneshot};
use futures::Async::Ready;
use futures::{Future, Poll, Stream as FStream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

pub type ConnectionId = u64;

#[derive(Clone)]
pub struct NewConnectionHandle<P> {
    new_con: strategies::NewConnectionHandle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    handle: Handle,
}

impl<P> NewConnectionHandle<P> {
    pub fn new(
        new_con: strategies::NewConnectionHandle,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> NewConnectionHandle<P> {
        NewConnectionHandle {
            new_con,
            connect_callback,
            handle: handle.clone(),
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture<P> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.connect_callback.clone(),
            &self.handle,
        )
    }
}

pub struct NewConnectionFuture<P> {
    new_con_recv: strategies::NewConnectionFuture,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    _marker: PhantomData<P>,
    handle: Handle,
}

impl<P> NewConnectionFuture<P> {
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> NewConnectionFuture<P> {
        NewConnectionFuture {
            new_con_recv,
            connect_callback,
            _marker: Default::default(),
            handle: handle.clone(),
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
        self.new_con_recv
            .poll()
            .map(|r| r.map(|v| Connection::new(v, self.connect_callback.clone(), &self.handle)))
    }
}

enum ConnectionState {
    UnAuthenticated {
        auth_recv: oneshot::Receiver<bool>,
        auth_send: Option<oneshot::Sender<bool>>,
    },
    Authenticated,
}

pub struct Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    con: strategies::Connection,
    state: ConnectionState,
    handle: Handle,
    connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    is_p2p: bool,
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        con: strategies::Connection,
        connect_callback: mpsc::UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        handle: &Handle,
    ) -> Connection<P> {
        let (send, auth_recv) = oneshot::channel();

        Connection {
            con,
            state: ConnectionState::UnAuthenticated {
                auth_recv,
                auth_send: Some(send),
            },
            handle: handle.clone(),
            connect_callback,
            is_p2p: false,
        }
    }

    pub fn set_p2p(&mut self, p2p: bool) {
        self.is_p2p = p2p;
    }

    pub fn new_stream(&mut self) -> NewStreamFuture<P> {
        NewStreamFuture::new(
            self.con.new_stream(),
            self.get_new_stream_handle(),
            self.connect_callback.clone(),
            self.is_p2p,
            &self.handle,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle<P> {
        NewStreamHandle::new(
            self.con.get_new_stream_handle(),
            self.connect_callback.clone(),
            self.is_p2p,
            &self.handle,
        )
    }
}

impl<P> FStream for Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Stream<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let state = match self.state {
                ConnectionState::Authenticated => {
                    return self.con.poll().map(|r| {
                        r.map(|o| {
                            o.map(|v| {
                                Stream::new(
                                    v,
                                    None,
                                    &self.handle,
                                    self.get_new_stream_handle(),
                                    self.connect_callback.clone(),
                                    self.is_p2p,
                                )
                            })
                        })
                    })
                }
                ConnectionState::UnAuthenticated {
                    ref mut auth_recv,
                    ref mut auth_send,
                } => {
                    loop {
                        let auth = match auth_recv.poll() {
                            Ok(Ready(auth)) => {
                                assert!(auth);
                                auth
                            }
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
                                    NewStreamHandle::new(
                                        self.con.get_new_stream_handle(),
                                        self.connect_callback.clone(),
                                        self.is_p2p,
                                        &self.handle,
                                    ),
                                    self.connect_callback.clone(),
                                    self.is_p2p,
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
