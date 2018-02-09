use errors::*;
use strategies::{AddressInformation, Connection, NewConnection, NewConnectionFuture,
                 NewConnectionHandle, NewSession, Strategy};
use config::Config;

use std::marker::PhantomData;
use std::net::SocketAddr;
use std::collections::{BinaryHeap, HashMap};
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cmp::Ordering;
use std::time::Duration;
use std::io::{self, Cursor};
use std::cmp::min;
use std::path::Path;

use futures::Async::{NotReady, Ready};
use futures::{self, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use futures::sync::mpsc::{channel, unbounded, Receiver, SendError, Sender, UnboundedReceiver,
                          UnboundedSender};
use futures::sync::oneshot;

use tokio_core::reactor::Handle;

use picoquic;

use bytes::BytesMut;

struct StrategyWrapper {
    context: picoquic::Context,
    handle: Handle,
    new_con: (UnboundedSender<Connection>, UnboundedReceiver<Connection>),
}

impl StrategyWrapper {
    fn new(
        listen_address: SocketAddr,
        cert_file: &Path,
        key_file: &Path,
        handle: Handle,
    ) -> Result<StrategyWrapper> {
        let config = picoquic::Config::server(
            cert_file
                .to_str()
                .expect("cert filename contains illegal characters"),
            key_file
                .to_str()
                .expect("key filename contains illegal characters"),
        );

        let context = picoquic::Context::new(&listen_address, &handle, config)?;

        let new_con = unbounded();

        Ok(StrategyWrapper {
            context,
            handle,
            new_con,
        })
    }

    fn poll_context(&mut self) {
        loop {
            match self.context.poll() {
                Ok(Ready(None)) => panic!("picoquic context returned None!"),
                Ok(Ready(Some(con))) => {
                    let send = self.new_con.0.clone();
                    self.handle
                        .spawn(ConnectionHandler::new(con, send).map_err(|e| error!("{:?}", e)));
                }
                Err(e) => panic!("{:?}", e),
                _ => return,
            }
        }
    }
}

impl Stream for StrategyWrapper {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_context();

        self.new_con
            .1
            .poll()
            .map_err(|_| "failed to poll new_con".into())
    }
}

impl NewConnection for StrategyWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        new_connection(
            self.context.new_connection(addr),
            self.new_con.0.clone(),
            self.handle.clone(),
        )
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(NewConnectionHandleWrapper::new(
            self.context.get_new_connection_handle(),
            self.handle.clone(),
            self.new_con.0.clone(),
        ))
    }
}

/// We don't propagate the underlying picoquic `Connection`, but we still need to poll this
/// connection for new `Stream`'s and that is done by the `ConnectionHandler`.
struct ConnectionHandler {
    con: picoquic::Connection,
    new_con_send: UnboundedSender<Connection>,
}

impl ConnectionHandler {
    fn new(
        con: picoquic::Connection,
        new_con_send: UnboundedSender<Connection>,
    ) -> ConnectionHandler {
        ConnectionHandler { con, new_con_send }
    }
}

impl Future for ConnectionHandler {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let stream = match try_ready!(self.con.poll()) {
                Some(s) => s,
                None => return Ok(Ready(())),
            };

            let _ = self.new_con_send
                .unbounded_send(Connection::new(ConnectionWrapper::new(
                    stream,
                    self.con.get_new_stream_handle(),
                )));
        }
    }
}

struct ConnectionWrapper {
    stream: picoquic::Stream,
    new_session: picoquic::NewStreamHandle,
}

impl ConnectionWrapper {
    fn new(stream: picoquic::Stream, new_session: picoquic::NewStreamHandle) -> ConnectionWrapper {
        ConnectionWrapper {
            stream,
            new_session,
        }
    }
}

impl Stream for ConnectionWrapper {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.stream.poll().map_err(|e| e.into())
    }
}

impl Sink for ConnectionWrapper {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.stream.start_send(item).map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete().map_err(|e| e.into())
    }
}

impl AddressInformation for ConnectionWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr()
    }
}

impl NewSession for ConnectionWrapper {
    fn new_session(&mut self) -> NewConnectionFuture {
        let handle = self.new_session.clone();
        let new_con = self.new_session
            .new_bidirectional_stream()
            .map(move |s| Connection::new(ConnectionWrapper::new(s, handle)))
            .map_err(|e| e.into());

        NewConnectionFuture::new(new_con)
    }
}

#[derive(Clone)]
struct NewConnectionHandleWrapper {
    new_con: picoquic::NewConnectionHandle,
    new_con_send: UnboundedSender<Connection>,
    handle: Handle,
}

impl NewConnectionHandleWrapper {
    fn new(
        new_con: picoquic::NewConnectionHandle,
        handle: Handle,
        new_con_send: UnboundedSender<Connection>,
    ) -> NewConnectionHandleWrapper {
        NewConnectionHandleWrapper {
            new_con,
            handle,
            new_con_send,
        }
    }
}

fn new_connection(
    new_con: picoquic::NewConnectionFuture,
    new_con_send: UnboundedSender<Connection>,
    handle: Handle,
) -> NewConnectionFuture {
    // Create a new `Connection` with picoquic. If the `Connection` is ready, we spawn
    // the `ConnectionHandler` and create a new `Stream`. The `Stream` will be returned
    // as the new hole_punch `Connection`.
    let future = new_con
        .and_then(move |mut con| {
            let new_stream_handle = con.get_new_stream_handle();
            let stream = con.new_bidirectional_stream();
            handle.spawn(ConnectionHandler::new(con, new_con_send).map_err(|e| error!("{:?}", e)));
            stream.map(move |s| Connection::new(ConnectionWrapper::new(s, new_stream_handle)))
        })
        .map_err(|e| e.into());

    NewConnectionFuture::new(future)
}

impl NewConnection for NewConnectionHandleWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        new_connection(
            self.new_con.new_connection(addr),
            self.new_con_send.clone(),
            self.handle.clone(),
        )
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(self.clone())
    }
}

pub fn init(handle: Handle, config: &Config) -> Result<Strategy> {
    Ok(Strategy::new(StrategyWrapper::new(
        config.udp_listen_address,
        &config.cert_file,
        &config.key_file,
        handle,
    )?))
}
