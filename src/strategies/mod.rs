use errors::*;
use protocol::Protocol;

use std::net::SocketAddr;

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Sink, StartSend, Stream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

mod udp_strat;

pub enum ConnectionType {
    /// The connection was created by an incoming connection from a remote address
    Incoming,
    /// The connection was created by connecting to a remote address
    Outgoing,
}

pub enum Strategy<P> {
    Udp(udp_strat::Server<P>),
}

impl<P> Stream for Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr, ConnectionType);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        //syntactic sugar
        struct Result<P>(Poll<Option<(Connection<P>, SocketAddr, ConnectionType)>, Error>);

        impl<P, C> From<Poll<Option<(C, SocketAddr, ConnectionType)>, Error>> for Result<P>
        where
            Connection<P>: From<C>,
        {
            fn from(value: Poll<Option<(C, SocketAddr, ConnectionType)>, Error>) -> Result<P> {
                match value {
                    Ok(Ready(Some(v))) => Result(Ok(Ready(Some((v.0.into(), v.1, v.2))))),
                    e @ _ => Result(e.map(|r| r.map(|_| None))),
                }
            }
        }

        let result: Result<P> = match self {
            &mut Strategy::Udp(ref mut server) => server.poll().into(),
        };

        result.0
    }
}

impl<P> ConnectTo for Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    fn connect(&mut self, addr: SocketAddr) {
        match self {
            &mut Strategy::Udp(ref mut server) => server.connect(addr),
        }
    }
}

impl<P> Strategy<P>
    where
    P: Serialize + for<'de> Deserialize<'de>,
{
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match self {
            &Strategy::Udp(ref server) => server.local_addr(),
        }
    }
}

pub enum Connection<P> {
    Udp(udp_strat::Connection<P>),
}

impl<P> Stream for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = Protocol<P>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self {
            &mut Connection::Udp(ref mut stream) => stream
                .poll()
                .chain_err(|| "error polling udp strategy connection"),
        }
    }
}

impl<P> Sink for Connection<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type SinkItem = Protocol<P>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self {
            &mut Connection::Udp(ref mut sink) => sink.start_send(item)
                .map_err(|_| "error at start_send on udp connection".into()),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self {
            &mut Connection::Udp(ref mut sink) => sink.poll_complete()
                .map_err(|_| "error at poll_complete on udp connection".into()),
        }
    }
}

enum FuturePollerState<F>
where
    F: Future,
    <F as Future>::Item: Stream,
    Error: From<<<F as Future>::Item as Stream>::Error>,
{
    Waiting(F),
    Finished(<F as Future>::Item),
}

pub struct FuturePoller<F>
where
    F: Future,
    <F as Future>::Item: Stream,
    Error: From<<<F as Future>::Item as Stream>::Error>,
{
    state: FuturePollerState<F>,
}

impl<F> FuturePoller<F>
where
    F: Future,
    <F as Future>::Item: Stream,
    Error: From<<<F as Future>::Item as Stream>::Error>,
{
    fn new(future: F) -> FuturePoller<F> {
        FuturePoller {
            state: FuturePollerState::Waiting(future),
        }
    }
}

impl<T, F> Stream for FuturePoller<F>
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

impl<F> From<F> for FuturePoller<F>
where
    F: Future,
    <F as Future>::Item: Stream,
    Error: From<<<F as Future>::Item as Stream>::Error>,
{
    fn from(val: F) -> FuturePoller<F> {
        FuturePoller::new(val)
    }
}

pub fn accept<P>(handle: &Handle) -> Vec<Strategy<P>> {
    let udp = udp_strat::accept_async(handle);

    vec![udp]
}

pub fn connect<P>(handle: &Handle) -> Vec<Strategy<P>> {
    let udp = udp_strat::connect_async(handle);

    vec![udp]
}

pub trait ConnectTo {
    fn connect(&mut self, addr: SocketAddr);
}
