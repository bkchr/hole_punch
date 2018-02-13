use errors::*;
use strategies;
use config::Config;
use incoming;
use protocol::Protocol;

use std::time::Duration;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::{Poll, Stream as FStream};
use futures::Async::{NotReady, Ready};
use futures::sync::oneshot;

use tokio_core::reactor::Handle;

use tokio_serde_json::{ReadJson, WriteJson};

use serde::{Deserialize, Serialize};

pub struct Context<P> {
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    incoming: FuturesUnordered<incoming::Handler<P>>,
    handle: Handle,
}

impl<P> Context<P> {
    fn new(handle: Handle, config: Config) -> Result<Context<P>> {
        let strats =
            strategies::init(handle, &config).chain_err(|| "error initializing the strategies")?;

        Ok(Context {
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
            incoming: FuturesUnordered::new(),
            handle,
        })
    }

    fn poll_strategies(&mut self) -> Result<()> {
        loop {
            match self.strategies.poll() {
                Ok(NotReady) => return Ok(()),
                Err(e) => return Err(e),
                Ok(Ready(Some((con, strat)))) => {
                    self.strategies.push(strat.into_future());
                    self.incoming.push(incoming::Handler::new(
                        Connection::new(con),
                        Duration::from_secs(1),
                        &self.handle,
                    ));
                }
                Ok(Ready(None)) => {
                    bail!("strategy returned None!");
                }
            }
        }
    }
}

impl<P> FStream for Context<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = ( Connection<P>, Stream<P> );
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.poll_strategies()?;

        loop {
            match try_ready!(self.incoming.poll()) {
                Some((con, stream)) => return Ok(Ready((con, stream))),
                // If the incoming handler returns `None`, then the incoming connection
                // does not need to be propagated.
                None => {}
            }
        }
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
}

impl<P> Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(con: strategies::Connection) -> Connection<P> {
        let (send, auth_recv) = oneshot::channel();

        Connection {
            con,
            state: ConnectionState::UnAuthenticated {
                auth_recv,
                auth_send: Some(send),
            },
        }
    }
}

impl<P> FStream for Connection<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = P;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let state = match self.state {
                ConnectionState::Authenticated => {
                    return self.poll().map(|r| r.map(|v| Stream::new(v, None)))
                }
                ConnectionState::UnAuthenticated {
                    ref mut auth_recv,
                    ref mut auth_send,
                } => {
                    loop {
                        let auth = match auth_recv.poll() {
                            Ok(Ready(Some(auth))) => {
                                assert!(auth);
                                auth
                            }
                            _ => false,
                        };

                        if auth {
                            break;
                        } else {
                            let stream = match try_ready!(self.poll()) {
                                Some(stream) => stream,
                                None => return Ok(Ready(None)),
                            };

                            // Take `auth_send` and return the new `Stream`.
                            // If `auth_send` is None, we don't propagate any longer `Stream`s,
                            // because only one `Stream` is allowed for unauthorized `Connection`s.
                            if let Some(send) = auth_send.take() {
                                return Ok(Ready(Some(Stream::new(stream, Some(send)))));
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

enum StreamState {
    Authenticated,
    UnAuthenticated(oneshot::Sender<bool>),
}

pub struct Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    stream: WriteJson<ReadJson<strategies::Stream, Protocol<P>>, Protocol<P>>,
    state: StreamState,
}

impl<P> Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(stream: strategies::Stream, auth_con: Option<oneshot::Sender<bool>>) -> Stream<P> {
        let state = match auth_con {
            Some(auth) => StreamState::UnAuthenticated(auth),
            None => StreamState::Authenticated,
        };

        Stream {
            stream: WriteJson::new(ReadJson::new(stream)),
            state,
        }
    }

    fn poll_authenticated(&mut self) -> Poll<Option<P>, Error> {
        loop {
            let msg = match try_ready!(self.con.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(None)),
            };

            match msg {
                Protocol::Embedded(msg) => return Ok(Ready(Some(msg))),
                _ => {}
            };
        }
    }

    fn poll_unauthenticated(&mut self) -> Poll<Option<P>, Error> {
        loop {
            let msg = match try_ready!(self.con.poll()) {
                Some(msg) => msg,
                None => return Ok(Ready(None)),
            };

            match msg {
                Protocol::Embedded(msg) => return Ok(Ready(Some(msg))),
                _ => {}
            };
        }
    }

    /// INTERNAL USE ONLY
    /// Can be used to poll the underlying `strategies::Stream` directly. This enables handlers to
    /// to process protocol messages.
    pub(crate) fn direct_poll(&mut self) -> Poll<Option<Protocol<P>>, Error> {
        self.stream.poll()
    }
}

impl<P> FStream for Stream<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = P;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.state {
            StreamState::Authenticated => self.poll_authenticated(),
            StreamState::UnAuthenticated(_) => self.poll_unauthenticated(),
        }
    }
}
