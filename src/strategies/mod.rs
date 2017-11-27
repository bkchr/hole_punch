use errors::*;
use protocol::Protocol;

use std::net::SocketAddr;

use futures::Async::{NotReady, Ready};
use futures::{Future, Poll, Stream};

use tokio_core::reactor::Handle;

use serde::{Deserialize, Serialize};

mod udp_strat;


pub enum Strategy<P> {
    Udp(udp_strat::Server<P>),
}

impl<P> Stream for Strategy<P>
where
    P: Serialize + for<'de> Deserialize<'de>,
{
    type Item = (Connection<P>, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        //syntactic sugar
        struct Result<P>(Poll<Option<(Connection<P>, SocketAddr)>, Error>);

        impl<P, C> From<Poll<Option<(C, SocketAddr)>, Error>> for Result<P>
        where
            Connection<P>: From<C>,
        {
            fn from(value: Poll<Option<(C, SocketAddr)>, Error>) -> Result<P> {
                match value {
                    Ok(Ready(Some(v))) => Result(Ok(Ready(Some((v.0.into(), v.1))))),
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
