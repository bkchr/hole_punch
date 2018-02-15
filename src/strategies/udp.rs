use errors::*;
use strategies::{AddressInformation, Connection, NewConnection, NewConnectionFuture,
                 NewConnectionHandle, NewStream, NewStreamFuture, Strategy, Stream, NewStreamHandle};
use config::Config;

use std::net::SocketAddr;
use std::path::Path;

use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

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
            .map(|r| r.map(|o| o.map(|v| Connection::new(ConnectionWrapper::new(v)))))
            .map_err(|e| e.into())
    }
}

impl NewConnection for StrategyWrapper {
    fn new_connection(&mut self, addr: SocketAddr) -> NewConnectionFuture {
        NewConnectionFuture::new(
            self.context
                .new_connection(addr)
                .map(|v| Connection::new(ConnectionWrapper::new(v)))
                .map_err(|e| e.into()),
        )
    }

    fn get_new_connection_handle(&self) -> NewConnectionHandle {
        NewConnectionHandle::new(NewConnectionHandleWrapper::new(
            self.context.get_new_connection_handle(),
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
        self.con
            .poll()
            .map(|r| {
                r.map(|o| {
                    o.map(|v| Stream::new(StreamWrapper::new(v, self.con.get_new_stream_handle())))
                })
            })
            .map_err(|e| e.into())
    }
}

impl NewStream for ConnectionWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let handle = self.con.get_new_stream_handle();

        NewStreamFuture::new(
            self.con
                .new_bidirectional_stream()
                .map(move |v| Stream::new(StreamWrapper::new(v, handle)))
                .map_err(|e| e.into()),
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(NewStreamHandleWrapper::new(
            self.con.get_new_stream_handle(),
        ))
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
    fn new(stream: picoquic::Stream, new_stream: picoquic::NewStreamHandle) -> StreamWrapper {
        StreamWrapper { stream, new_stream }
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
        let handle = self.new_stream.clone();
        let new_con = self.new_stream
            .new_bidirectional_stream()
            .map(move |s| Stream::new(StreamWrapper::new(s, handle)))
            .map_err(|e| e.into());

        NewStreamFuture::new(new_con)
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(NewStreamHandleWrapper::new(self.new_stream.clone()))
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
                .map(|v| Connection::new(ConnectionWrapper::new(v)))
                .map_err(|e| e.into()),
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
        let handle = self.new_stream.clone();

        NewStreamFuture::new(
            self.new_stream
                .new_bidirectional_stream()
                .map(|v| Stream::new(StreamWrapper::new(v, handle)))
                .map_err(|e| e.into()),
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(self.clone())
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
