use context::PassStreamToContext;
use error::*;
use strategies::{self, NewConnection, NewStream};
use stream::{NewStreamFuture, NewStreamHandle, Stream, StreamHandle};

use std::net::SocketAddr;

use futures::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender}, oneshot,
    }, Async::Ready,
    Future, Poll, Stream as FStream,
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
}

impl<P> NewConnectionHandle<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        new_con: strategies::NewConnectionHandle,
        pass_stream_to_context: PassStreamToContext<P>,
        handle: &Handle,
    ) -> NewConnectionHandle<P> {
        NewConnectionHandle {
            new_con,
            pass_stream_to_context,
            handle: handle.clone(),
        }
    }

    pub fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture<P> {
        NewConnectionFuture::new(
            self.new_con.new_connection(addr),
            self.pass_stream_to_context.clone(),
            &self.handle,
        )
    }
}

pub struct NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    new_con_recv: strategies::NewConnectionFuture,
    pass_stream_to_context: PassStreamToContext<P>,
    handle: Handle,
}

impl<P> NewConnectionFuture<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        new_con_recv: strategies::NewConnectionFuture,
        pass_stream_to_context: PassStreamToContext<P>,
        handle: &Handle,
    ) -> NewConnectionFuture<P> {
        NewConnectionFuture {
            new_con_recv,
            pass_stream_to_context,
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
        self.new_con_recv.poll().map(|r| {
            r.map(|v| Connection::new(v, self.pass_stream_to_context.clone(), &self.handle))
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

pub struct Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    con: strategies::Connection,
    state: ConnectionState,
    handle: Handle,
    pass_stream_to_context: PassStreamToContext<P>,
    connect_peers: (
        UnboundedReceiver<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
        ConnectPeers<P>,
    ),
    is_p2p: bool,
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        con: strategies::Connection,
        pass_stream_to_context: PassStreamToContext<P>,
        handle: &Handle,
    ) -> Connection<P> {
        let (send, auth_recv) = oneshot::channel();
        let (connect_peers_send, connect_peers_recv) = mpsc::unbounded();
        let connect_peers = ConnectPeers::new(connect_peers_send);

        Connection {
            con,
            state: ConnectionState::UnAuthenticated {
                auth_recv,
                auth_send: Some(send),
            },
            handle: handle.clone(),
            pass_stream_to_context,
            connect_peers: (connect_peers_recv, connect_peers),
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
            self.pass_stream_to_context.clone(),
            self.connect_peers.1.clone(),
            self.is_p2p,
            &self.handle,
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle<P> {
        NewStreamHandle::new(
            self.con.get_new_stream_handle(),
            self.pass_stream_to_context.clone(),
            self.connect_peers.1.clone(),
            self.is_p2p,
            &self.handle,
        )
    }

    fn poll_impl(&mut self) -> Poll<(), Error> {
        loop {
            let state = match self.state {
                ConnectionState::Authenticated => loop {
                    let stream = match try_ready!(self.con.poll()) {
                        Some(stream) => stream,
                        None => return Ok(Ready(())),
                    };

                    self.pass_stream_to_context.pass_stream(Stream::new(
                        stream,
                        None,
                        &self.handle,
                        self.get_new_stream_handle(),
                        self.pass_stream_to_context.clone(),
                        self.connect_peers.1.clone(),
                        self.is_p2p,
                    ));
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
                                None => return Ok(Ready(())),
                            };

                            // Take `auth_send` and return the new `Stream`.
                            // If `auth_send` is None, we don't propagate any longer `Stream`s,
                            // because only one `Stream` is allowed for unauthorized `Connection`s.
                            if let Some(send) = auth_send.take() {
                                self.pass_stream_to_context.pass_stream(Stream::new(
                                    stream,
                                    Some(send),
                                    &self.handle,
                                    self.get_new_stream_handle(),
                                    self.pass_stream_to_context.clone(),
                                    self.connect_peers.1.clone(),
                                    self.is_p2p,
                                ));
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

impl<P> Future for Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.poll_impl() {
            Err(e) => {
                println!("{:?}", e);
                Ok(Ready(()))
            }
            r @ _ => r.map_err(|_| ()),
        }
    }
}

#[derive(Clone)]
pub struct ConnectPeers<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    sender: UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
}

impl<P> ConnectPeers<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(
        sender: UnboundedSender<(Vec<SocketAddr>, ConnectionId, StreamHandle<P>)>,
    ) -> ConnectPeers<P> {
        ConnectPeers { sender }
    }

    pub fn connect(
        &mut self,
        addresses: Vec<SocketAddr>,
        con_id: ConnectionId,
        stream_handle: StreamHandle<P>,
    ) {
        let _ = self
            .sender
            .unbounded_send((addresses, con_id, stream_handle));
    }
}
