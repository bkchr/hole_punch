use authenticator::Authenticator;
use config::Config;
use error::*;
use strategies::{
    Connection, ConnectionId, GetConnectionId, LocalAddressInformation, NewConnection,
    NewConnectionFuture, NewConnectionHandle, NewStream, NewStreamFuture, NewStreamHandle,
    PeerAddressInformation, Strategy, Stream,
};

use std::net::SocketAddr;
use std::time::Duration;

use futures::{Future, Poll, Sink, StartSend, Stream as FStream};

use picoquic;

use bytes::{Bytes, BytesMut};

use tokio::runtime::TaskExecutor;

struct StrategyWrapper {
    context: picoquic::Context,
}

impl StrategyWrapper {
    fn new(
        hconfig: &Config,
        handle: TaskExecutor,
        authenticator: Authenticator,
    ) -> Result<StrategyWrapper> {
        let mut config = picoquic::Config::clone_from(&hconfig.quic_config);

        config.enable_keep_alive(Duration::from_secs(15));
        config.enable_client_authentication();
        config.set_verify_certificate_handler(authenticator);

        let context = picoquic::Context::new(&hconfig.quic_listen_address, handle, config)?;

        Ok(StrategyWrapper { context })
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
                .new_connection(addr, "hole_punch")
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

impl LocalAddressInformation for StrategyWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.context.local_addr()
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
                    o.map(|v| {
                        Stream::new(StreamWrapper::new(
                            v,
                            self.con.get_new_stream_handle(),
                            self.con.id(),
                        ))
                    })
                })
            })
            .map_err(|e| e.into())
    }
}

impl NewStream for ConnectionWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let handle = self.con.get_new_stream_handle();
        let id = self.con.id();

        NewStreamFuture::new(
            self.con
                .new_bidirectional_stream()
                .map(move |v| Stream::new(StreamWrapper::new(v, handle, id)))
                .map_err(|e| e.into()),
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(NewStreamHandleWrapper::new(
            self.con.get_new_stream_handle(),
            self.con.id(),
        ))
    }
}

impl LocalAddressInformation for ConnectionWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.con.local_addr()
    }
}

impl PeerAddressInformation for ConnectionWrapper {
    fn peer_addr(&self) -> SocketAddr {
        self.con.peer_addr()
    }
}

impl GetConnectionId for ConnectionWrapper {
    fn connection_id(&self) -> ConnectionId {
        self.con.id()
    }
}

struct StreamWrapper {
    stream: picoquic::Stream,
    new_stream: picoquic::NewStreamHandle,
    con_id: ConnectionId,
}

impl StreamWrapper {
    fn new(
        stream: picoquic::Stream,
        new_stream: picoquic::NewStreamHandle,
        con_id: ConnectionId,
    ) -> StreamWrapper {
        StreamWrapper {
            stream,
            new_stream,
            con_id,
        }
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
    type SinkItem = Bytes;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.stream.start_send(item).map_err(|e| e.into())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.stream.poll_complete().map_err(|e| e.into())
    }
}

impl LocalAddressInformation for StreamWrapper {
    fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }
}

impl PeerAddressInformation for StreamWrapper {
    fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}

impl GetConnectionId for StreamWrapper {
    fn connection_id(&self) -> ConnectionId {
        self.con_id
    }
}

impl NewStream for StreamWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let handle = self.new_stream.clone();
        let id = self.con_id;

        let new_con = self
            .new_stream
            .new_bidirectional_stream()
            .map(move |s| Stream::new(StreamWrapper::new(s, handle, id)))
            .map_err(|e| e.into());

        NewStreamFuture::new(new_con)
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(NewStreamHandleWrapper::new(
            self.new_stream.clone(),
            self.con_id,
        ))
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
                .new_connection(addr, "hole_punch")
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
    con_id: ConnectionId,
}

impl NewStreamHandleWrapper {
    fn new(new_stream: picoquic::NewStreamHandle, con_id: ConnectionId) -> NewStreamHandleWrapper {
        NewStreamHandleWrapper { new_stream, con_id }
    }
}

impl NewStream for NewStreamHandleWrapper {
    fn new_stream(&mut self) -> NewStreamFuture {
        let handle = self.new_stream.clone();
        let id = self.con_id;

        NewStreamFuture::new(
            self.new_stream
                .new_bidirectional_stream()
                .map(move |v| Stream::new(StreamWrapper::new(v, handle, id)))
                .map_err(|e| e.into()),
        )
    }

    fn get_new_stream_handle(&self) -> NewStreamHandle {
        NewStreamHandle::new(self.clone())
    }
}

pub fn init(
    handle: TaskExecutor,
    config: &Config,
    authenticator: Authenticator,
) -> Result<Strategy> {
    Ok(Strategy::new(StrategyWrapper::new(
        config,
        handle,
        authenticator,
    )?))
}
