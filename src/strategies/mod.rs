use errors::*;
use udp;

use std::net::SocketAddr;

use futures::Async::{NotReady, Ready};
use futures::future::FutureResult;
use futures::{Future, Poll, Stream};

use tokio_core::reactor::Handle;

pub enum Strategy {
    Udp(FuturePoller<udp::UdpServer, FutureResult<udp::UdpServer, Error>>),
}

impl Stream for Strategy {
    type Item = (Connection, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Strategy::Udp(ref mut server) => server
                .poll()
                .chain_err(|| "ahh")
                .map(|v| v.map(|o| o.map(|v| (v.0.into(), v.1)))),
        }
    }
}

pub enum Connection {
    Udp(udp::UdpServerStream),
}

impl Stream for Connection {
    type Item = Vec<u8>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Connection::Udp(ref mut stream) => stream.poll().map_err(|_| "error".into()),
        }
    }
}

impl From<udp::UdpServerStream> for Connection {
    fn from(value: udp::UdpServerStream) -> Connection {
        Connection::Udp(value)
    }
}

enum FuturePollerState<T, F>
where
    T: Stream,
    Error: From<<T as Stream>::Error>,
    F: Future<Item = T>,
{
    Waiting(F),
    Finished(T),
}

pub struct FuturePoller<T, F>
where
    T: Stream,
    Error: From<<T as Stream>::Error>,
    F: Future<Item = T>,
{
    state: FuturePollerState<T, F>,
}

impl<T, F> FuturePoller<T, F>
where
    T: Stream,
    Error: From<<T as Stream>::Error>,
    F: Future<Item = T>,
{
    fn new(future: F) -> FuturePoller<T, F> {
        FuturePoller {
            state: FuturePollerState::Waiting(future),
        }
    }
}

impl<T, F> Stream for FuturePoller<T, F>
where
    T: Stream,
    Error: From<<T as Stream>::Error>,
    F: Future<Item = T>,
    Error: From<<F as Future>::Error>,
{
    type Error = Error;
    type Item = T::Item;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let new_state = match self.state {
                FuturePollerState::Waiting(ref mut w) => match w.poll() {
                    Ok(Ready(v)) => FuturePollerState::Finished(v),
                    Ok(NotReady) => return Ok(NotReady),
                    Err(e) => return Err(e.into()),
                },
                FuturePollerState::Finished(ref mut v) => return v.poll().map_err(|e| e.into()),
            };

            self.state = new_state;
        }
    }
}

pub fn accept(handle: &Handle) -> Vec<Strategy> {
    let udp = udp::accept_async(([0, 0, 0, 0], 22222).into(), handle, 4);
    let udp = Strategy::Udp(FuturePoller::new(udp));

    vec![udp]
}
