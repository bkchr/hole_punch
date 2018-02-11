use errors::*;
use strategies::{AddressInformation, Connection, NewConnection, NewConnectionFuture,
                 NewConnectionHandle, NewStream, NewStreamFuture, Strategy, Stream};
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
use futures::{self, AsyncSink, Future, Poll, Sink, StartSend, Stream as FStream};
use futures::sync::mpsc::{channel, unbounded, Receiver, SendError, Sender, UnboundedReceiver,
                          UnboundedSender};
use futures::sync::oneshot;

use tokio_core::reactor::Handle;

use picoquic;

use bytes::BytesMut;

struct StrategyWrapper {
    context: picoquic::Context,
    handle: Handle,
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

        Ok(StrategyWrapper { context, handle })
    }
}

impl FStream for StrategyWrapper {
    type Item = Connection;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.context
            .poll()
            .map(|r| r.map(|v| ConnectionWrapper::new))
    }
}

impl NewConnection for StrategyWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        NewConnectionFuture::new(
            self.context
                .new_connection(addr)
                .map(|v| ConnectionWrapper::new),
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

struct ConnectionWrapper {
    con: picoquic::Connection,
}

impl ConnectionWrapper {
    fn new(con: picoquic::Connection) -> ConnectionWrapper {
        ConnectionWrapper { con }
    }
}

impl FStream for ConnectionWrapper {
    type Item = Stream;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.con.poll().map(|r| {
            r.map(|v| Stream::new(StreamWrapper::new(v, self.con.get_new_stream_handle())))
        })
    }
}

impl NewStream for ConnectionWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        NewStreamFuture::new(
            self.con
                .new_bidirectional_stream()
                .map(|v| StreamWrapper::new(v, self.con.get_new_stream_handle())),
        )
    }
}

impl AddressInformation for ConnectionWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.con.local_addr()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.con.peer_addr()
    }
}

struct StreamWrapper {
    stream: picoquic::Stream,
    new_stream: picoquic::NewStreamHandle,
}

impl StreamWrapper {
    fn new(stream: picoquic::Stream, new_stream: picoquic::NewStreamHandle) -> ConnectionWrapper {
        ConnectionWrapper { stream, new_stream }
    }
}

impl FStream for StreamWrapper {
    type Item = BytesMut;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.stream.poll().map_err(|e| e.into())
    }
}

impl Sink for StreamWrapper {
    type SinkItem = BytesMut;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.stream.start_send(item).map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete().map_err(|e| e.into())
    }
}

impl AddressInformation for StreamWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}

impl NewStream for StreamWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let handle = self.new_session.clone();
        let new_con = self.new_session
            .new_bidirectional_stream()
            .map(move |s| Stream::new(StreamWrapper::new(s, handle)))
            .map_err(|e| e.into());

        NewStreamFuture::new(new_con)
    }
}

#[derive(Clone)]
struct NewConnectionHandleWrapper {
    new_con: picoquic::NewConnectionHandle,
}

impl NewConnectionHandleWrapper {
    fn new(new_con: picoquic::NewConnectionHandle) -> NewConnectionHandleWrapper {
        NewConnectionHandleWrapper { new_con }
    }
}

impl NewConnection for NewConnectionHandleWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        NewConnectionFuture::new(
            self.new_con
                .new_connection(addr)
                .map(|v| ConnectionWrapper::new),
        )
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(self.clone())
    }
}

#[derive(Clone)]
struct NewStreamHandleWrapper {
    new_stream: picoquic::NewStreamHandle,
}

impl NewStreamHandleWrapper {
    fn new(new_stream: picoquic::NewStreamHandle) -> NewStreamHandleWrapper {
        NewStreamHandleWrapper { new_stream }
    }
}

impl NewStream for NewStreamHandleWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        NewStreamFuture::new(
            self.new_stream
                .new_bidirectional_stream()
                .map(|v| StreamWrapper::new(v, self.new_stream.clone())),
        )
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
