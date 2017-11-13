use errors::*;

use std::{fmt, io};
use std::net::SocketAddr;
use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cmp::min;

use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use tokio_io::{AsyncRead, AsyncWrite};

use futures::Async::{NotReady, Ready};
use futures::future::FutureResult;
use futures::{AsyncSink, StartSend};
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::{self, IntoFuture, Poll, Sink, Stream};

/// Represents an `UdpStream` that is connected to a remote socket.
pub struct UdpConnectStream {
    /// The underlying socket
    socket: UdpSocket,
}

impl UdpConnectStream {
    pub fn new(socket: UdpSocket) -> UdpConnectStream {
        UdpConnectStream { socket }
    }

    pub fn port(&self) -> Result<u16> {
        self.socket.local_addr().map(|a| a.port()).chain_err(|| "error")
    }
}

impl io::Write for UdpConnectStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for UdpConnectStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.recv(buf)
    }
}

impl AsyncRead for UdpConnectStream {}

impl AsyncWrite for UdpConnectStream {
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}

/// Represents an incoming connection
struct UdpConnection {
    /// This sender is used to forward data to the stream object
    sender: Sender<Vec<u8>>,
    /// This receiver is used to forward data from the stream object
    recv: Receiver<Vec<u8>>,
}

impl UdpConnection {
    /// Creates a new `UdpConnection`
    ///
    /// * `recv` - The receiver to receive data from the connected `UdpAcceptStream`
    /// * `sender` - The sender to send data to the connected `UdpAcceptStream`
    fn new(recv: Receiver<Vec<u8>>, sender: Sender<Vec<u8>>) -> UdpConnection {
        UdpConnection { sender, recv }
    }

    /// Forwards received data to the connected `UdpAcceptStream`
    fn recv(&mut self, data: Vec<u8>) {
        // if we see an error here, abort the function.
        // this connection will be dropped in the next `poll` call of `UdpServer`.
        // the check is only required for start_send, but to mute the warning, check the result of
        // poll_complete, too.
        if self.sender.start_send(data).is_err() || self.sender.poll_complete().is_err() {
            return;
        }
    }

    /// Checks if the connected `UdpAcceptStream` wants to send data
    ///
    /// # Return value
    ///
    /// * `Ok(Some(d))` - The stream wants to send data
    /// * `Ok(None)` - The stream does not want to send data
    /// * `Err(_)` - The stream was dropped and this connection can also be dropped
    fn send(&mut self) -> Result<Option<Vec<u8>>> {
        match self.recv.poll() {
            Ok(Ready(Some(data))) => Ok(Some(data)),
            Ok(NotReady) => Ok(None),
            // Err(_) | Ok(Ready(None)), we interpret both as that the stream was dropped
            _ => bail!("stream dropped"),
        }
    }
}

/// The `UdpServer` checks the socket for incoming data, routes the data to the appropriate
/// `UdpConnection` and sends data from `UdpConnection` over the socket.
pub struct UdpServer {
    /// The socket the server is listening on
    socket: UdpSocket,
    /// All active connections we the server is handling
    connections: HashMap<SocketAddr, UdpConnection>,
    /// Temp buffer for receiving messages
    buf: Vec<u8>,
    /// The buffer size of the `UdpConnection` and `UdpAcceptStream` channels
    buffer_size: usize,
}

impl UdpServer {
    /// Creates a new instance of the `UdpServer`
    ///
    /// * `socket` - The `UdpSocket` this server should use.
    /// * `channel_buffer` - Defines the buffer size of the channel that connects
    ///                      `UdpConnection` and `UdpAcceptStream`. Both sides drop data/return
    ///                      `WouldBlock` if the channel is full.
    fn new(socket: UdpSocket, channel_buffer: usize) -> UdpServer {
        UdpServer {
            socket,
            buf: vec![0; 1024],
            connections: HashMap::new(),
            buffer_size: channel_buffer,
        }
    }

    /// Checks all connections if they want to send data.
    /// While checking for data to send, connections that return a `Err(_)` are dropped from the
    /// hash map.
    fn send_data(&mut self) {
        // the borrow checker does not want 2 mutable references to self, but with this trick it
        // works
        fn retain(connections: &mut HashMap<SocketAddr, UdpConnection>, socket: &mut UdpSocket) {
            connections.retain(|addr, c| loop {
                let _ = match c.send() {
                    Ok(Some(ref data)) => socket.send_to(&data, &addr),
                    Ok(None) => return true,
                    _ => return false,
                };
            });
        }

        retain(&mut self.connections, &mut self.socket);
    }

    /// Creates a new `UdpConnection` and the connected `UdpAcceptStream`
    fn create_connection_and_stream(buffer_size: usize) -> (UdpConnection, UdpAcceptStream) {
        let (con_sender, con_receiver) = channel(buffer_size);
        let (stream_sender, stream_receiver) = channel(buffer_size);

        (
            UdpConnection::new(stream_receiver, con_sender),
            UdpAcceptStream::new(con_receiver, stream_sender),
        )
    }

    pub fn connect(&mut self, addr: SocketAddr) -> UdpAcceptStream {
        let (con, stream) = Self::create_connection_and_stream(self.buffer_size);
        self.connections.insert(addr, con);
        stream
    }
}

impl Stream for UdpServer {
    type Item = (UdpAcceptStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.send_data();

        loop {
            let (len, addr) = try_nb!(self.socket.recv_from(&mut self.buf));

            // check if the address is already in our connections map
            match self.connections.entry(addr) {
                Occupied(mut entry) => entry.get_mut().recv(self.buf[..len].to_vec()),
                Vacant(entry) => {
                    let (con, stream) = Self::create_connection_and_stream(self.buffer_size);
                    entry.insert(con).recv(self.buf[..len].to_vec());
                    return Ok(Ready(Some((stream, addr))));
                }
            };
        }
    }
}

/// UdpStream that is created by a `UdpServer` and is connected to a `UdpConnection`.
pub struct UdpAcceptStream {
    /// The sender to send data to the connected `UdpConnection` and effectively over the socket
    sender: Sender<Vec<u8>>,
    /// The receiver to recv data from the connected `UdpConnection`
    receiver: Receiver<Vec<u8>>,
}

impl UdpAcceptStream {
    /// Creates a new UdpAcceptStream
    fn new(receiver: Receiver<Vec<u8>>, sender: Sender<Vec<u8>>) -> UdpAcceptStream {
        UdpAcceptStream { receiver, sender }
    }
}

impl Stream for UdpAcceptStream {
    type Item = Vec<u8>;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.receiver.poll()
    }
}

impl Sink for UdpAcceptStream {
    type SinkItem = Vec<u8>;
    type SinkError = <Sender<Vec<u8>> as Sink>::SinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.sender.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sender.poll_complete()
    }
}

fn to_io_error<E: fmt::Debug>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", error))
}

impl io::Write for UdpAcceptStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let AsyncSink::NotReady(_) = self.start_send(buf.to_vec()).map_err(to_io_error)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        self.poll_complete().map_err(to_io_error)?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for UdpAcceptStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let res = self.poll().map_err(to_io_error)?;

        match res {
            NotReady => Err(io::ErrorKind::WouldBlock.into()),
            Ready(Some(data)) => {
                // If buf is too small, elements will get lost
                // TODO: maybe integrate tmp buffer for 'lost' elements.
                let len = min(buf.len(), data.len());
                &buf[..len].copy_from_slice(&data.as_slice()[..len]);
                Ok(len)
            }
            Ready(None) => Ok(0),
        }
    }
}

impl AsyncRead for UdpAcceptStream {}

impl AsyncWrite for UdpAcceptStream {
    fn shutdown(&mut self) -> io::Result<futures::Async<()>> {
        Ok(Ready(()))
    }
}

pub fn connect_async(
    connect: SocketAddr,
    handle: &Handle,
) -> FutureResult<UdpConnectStream, Error> {
    UdpSocket::bind(&([0, 0, 0, 0], 0).into(), handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| {
            socket
                .connect(&connect)
                .chain_err(|| format!("error connecting to {:?}", connect))
                .map(|_| socket)
        })
        .and_then(|socket| Ok(UdpConnectStream::new(socket)))
        .into_future()
}

pub fn accept_async(
    listen_addr: SocketAddr,
    handle: &Handle,
    channel_buffer: usize,
) -> FutureResult<UdpServer, Error> {
    UdpSocket::bind(&listen_addr, handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| Ok(UdpServer::new(socket, channel_buffer)))
        .into_future()
}

pub fn connect_and_accept_async(
    connect: SocketAddr,
    listen_addr: SocketAddr,
    handle: &Handle,
    channel_buffer: usize,
) -> FutureResult<(UdpServer, UdpAcceptStream), Error> {
    UdpSocket::bind(&listen_addr, handle)
        .chain_err(|| "error binding to socket")
        .and_then(|socket| Ok(UdpServer::new(socket, channel_buffer)))
        .and_then(|mut server| { let stream = server.connect(connect); Ok((server, stream))})
        .into_future()
}
