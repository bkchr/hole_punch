use connection::{NewConnectionFuture, NewConnectionHandle};
use context::PassStreamToContext;
use error::*;
use protocol::{BuildConnectionToPeer as BuildConnectionToPeerProtocol, StreamHello};
use strategies::{self, LocalAddressInformation, PeerAddressInformation};
use stream::{NewStreamFuture, NewStreamHandle, ProtocolStream, Stream};
use timeout::Timeout;
use PubKeyHash;

use std::{net::SocketAddr, time::Duration};

use tokio_core::reactor::Handle;

use pnet_datalink::interfaces;

use itertools::Itertools;

use futures::{
    stream::{futures_unordered, FuturesUnordered}, Async::{NotReady, Ready}, Future, Poll, Sink,
    Stream as FStream,
};

use state_machine_future::RentToOwn;

/// `BuildConnectionToPeer` is used by the initiating side of the new connection.
#[derive(StateMachineFuture)]
pub enum BuildConnectionToPeer {
    #[state_machine_future(start, transitions(WaitingForInternetAddressInformation))]
    WaitingForProxyStream {
        proxy_stream: NewStreamFuture,
        timeout: Duration,
        handle: Handle,
        new_con_handle: NewConnectionHandle,
        new_stream_handle: NewStreamHandle,
        peer_identifier: PubKeyHash,
        local_peer_identifier: PubKeyHash,
    },
    #[state_machine_future(transitions(WaitingForExchangeAddressInformation))]
    WaitingForInternetAddressInformation {
        proxy_stream: ProtocolStream<BuildConnectionToPeerProtocol>,
        timeout: Duration,
        handle: Handle,
        new_con_handle: NewConnectionHandle,
        new_stream_handle: NewStreamHandle,
        peer_identifier: PubKeyHash,
        local_peer_identifier: PubKeyHash,
    },
    #[state_machine_future(transitions(WaitingForConnection))]
    WaitingForExchangeAddressInformation {
        proxy_stream: ProtocolStream<BuildConnectionToPeerProtocol>,
        timeout: Duration,
        handle: Handle,
        new_con_handle: NewConnectionHandle,
        new_stream_handle: NewStreamHandle,
        peer_identifier: PubKeyHash,
        local_peer_identifier: PubKeyHash,
    },
    #[state_machine_future(transitions(WaitingForStream, ProxyStream))]
    WaitingForConnection {
        timeout: Timeout,
        new_cons: FuturesUnordered<NewConnectionFuture>,
        handle: Handle,
        proxy_stream: ProtocolStream<BuildConnectionToPeerProtocol>,
        local_peer_identifier: PubKeyHash,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
    },
    #[state_machine_future(transitions(ConnectionBuilt, ProxyStream))]
    WaitingForStream {
        new_stream: NewStreamFuture,
        proxy_stream: ProtocolStream<BuildConnectionToPeerProtocol>,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
    },
    #[state_machine_future(transitions(ConnectionBuilt))]
    ProxyStream {
        proxy_stream: ProtocolStream<BuildConnectionToPeerProtocol>,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
    },
    #[state_machine_future(ready)]
    ConnectionBuilt(Stream),
    #[state_machine_future(error)]
    PeerToPeerError(Error),
}

fn get_interface_addresses(local_addr: SocketAddr) -> Vec<SocketAddr> {
    interfaces()
        .iter()
        .map(|v| v.ips.clone())
        .concat()
        .iter()
        .map(|v| v.ip())
        .filter(|ip| !ip.is_loopback())
        .map(|ip| (ip, local_addr.port()).into())
        .collect_vec()
}

impl BuildConnectionToPeer {
    pub fn new(
        local_peer_identifier: PubKeyHash,
        peer_identifier: PubKeyHash,
        new_con_handle: NewConnectionHandle,
        mut new_stream_handle: NewStreamHandle,
        timeout: Duration,
        handle: Handle,
    ) -> BuildConnectionToPeerFuture {
        let proxy_stream = new_stream_handle.new_stream_with_hello(
            StreamHello::ProxyBuildConnectionToPeer(peer_identifier.clone()),
        );

        BuildConnectionToPeer::start(
            proxy_stream,
            timeout,
            handle,
            new_con_handle,
            new_stream_handle,
            peer_identifier,
            local_peer_identifier,
        )
    }
}

impl PollBuildConnectionToPeer for BuildConnectionToPeer {
    fn poll_waiting_for_proxy_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForProxyStream>,
    ) -> Poll<AfterWaitingForProxyStream, Error> {
        let proxy_stream = try_ready!(wait.proxy_stream.poll());
        let wait = wait.take();
        transition!(WaitingForInternetAddressInformation {
            proxy_stream: proxy_stream.into(),
            handle: wait.handle,
            new_stream_handle: wait.new_stream_handle,
            new_con_handle: wait.new_con_handle,
            timeout: wait.timeout,
            peer_identifier: wait.peer_identifier,
            local_peer_identifier: wait.local_peer_identifier,
        })
    }

    fn poll_waiting_for_internet_address_information<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForInternetAddressInformation>,
    ) -> Poll<AfterWaitingForInternetAddressInformation, Error> {
        let msg = match try_ready!(wait.proxy_stream.poll()) {
            Some(msg) => msg,
            None => bail!("Stream closed while waiting for internet address information."),
        };

        let addr = match msg {
            BuildConnectionToPeerProtocol::InternetAddressInformation(addr) => addr,
            _ => bail!("Received illegal message while waiting for internet address information"),
        };

        let mut wait = wait.take();
        let mut addresses =
            get_interface_addresses(wait.proxy_stream.get_ref().get_ref().get_ref().local_addr());
        addresses.push(addr);

        wait.proxy_stream
            .start_send(BuildConnectionToPeerProtocol::ExchangeAddressInformation(
                addresses,
            ))?;
        wait.proxy_stream.poll_complete()?;

        transition!(WaitingForExchangeAddressInformation {
            proxy_stream: wait.proxy_stream,
            handle: wait.handle,
            new_stream_handle: wait.new_stream_handle,
            new_con_handle: wait.new_con_handle,
            timeout: wait.timeout,
            peer_identifier: wait.peer_identifier,
            local_peer_identifier: wait.local_peer_identifier,
        })
    }

    fn poll_waiting_for_exchange_address_information<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForExchangeAddressInformation>,
    ) -> Poll<AfterWaitingForExchangeAddressInformation, Error> {
        let msg = match try_ready!(wait.proxy_stream.poll()) {
            Some(msg) => msg,
            None => bail!("Stream closed while waiting for exchange address information."),
        };

        let addresses = match msg {
            BuildConnectionToPeerProtocol::ExchangeAddressInformation(addresses) => addresses,
            _ => bail!("Received illegal message while waiting for exchange address information"),
        };

        let mut wait = wait.take();
        let timeout = Timeout::new(wait.timeout, &wait.handle);
        let new_cons = futures_unordered(
            addresses
                .into_iter()
                .map(|a| wait.new_con_handle.new_connection(a)),
        );

        transition!(WaitingForConnection {
            proxy_stream: wait.proxy_stream,
            timeout,
            new_cons,
            handle: wait.handle,
            local_peer_identifier: wait.local_peer_identifier,
            peer_identifier: wait.peer_identifier,
            new_stream_handle: wait.new_stream_handle,
        })
    }

    fn poll_waiting_for_connection<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForConnection>,
    ) -> Poll<AfterWaitingForConnection, Error> {
        let mut new_con = match wait.new_cons.poll() {
            Ok(Ready(Some(con))) => con,
            Err(_) | Ok(Ready(None)) => {
                let wait = wait.take();

                transition!(ProxyStream {
                    proxy_stream: wait.proxy_stream,
                    peer_identifier: wait.peer_identifier,
                    new_stream_handle: wait.new_stream_handle,
                })
            }
            Ok(NotReady) => return Ok(NotReady),
        };

        let wait = wait.take();
        let new_stream =
            new_con.new_stream_with_hello(StreamHello::User(wait.local_peer_identifier.clone()));

        wait.handle.spawn(new_con);

        transition!(WaitingForStream {
            new_stream,
            proxy_stream: wait.proxy_stream,
            peer_identifier: wait.peer_identifier,
            new_stream_handle: wait.new_stream_handle,
        })
    }

    fn poll_waiting_for_stream<'a>(
        wait: &'a mut RentToOwn<'a, WaitingForStream>,
    ) -> Poll<AfterWaitingForStream, Error> {
        let stream = match wait.new_stream.poll() {
            Ok(Ready(stream)) => stream,
            Err(_) => {
                let wait = wait.take();

                transition!(ProxyStream {
                    proxy_stream: wait.proxy_stream,
                    peer_identifier: wait.peer_identifier,
                    new_stream_handle: wait.new_stream_handle,
                })
            }
            Ok(NotReady) => return Ok(NotReady),
        };

        transition!(ConnectionBuilt(stream))
    }

    fn poll_proxy_stream<'a>(
        wait: &'a mut RentToOwn<'a, ProxyStream>,
    ) -> Poll<AfterProxyStream, Error> {
        let mut wait = wait.take();

        wait.proxy_stream
            .start_send(BuildConnectionToPeerProtocol::ProxyConnection)?;
        wait.proxy_stream.poll_complete()?;

        transition!(ConnectionBuilt(Stream::new(
            wait.proxy_stream,
            wait.peer_identifier,
            wait.new_stream_handle,
            true
        )))
    }
}

fn create_poke_connections(
    new_con_handle: &mut NewConnectionHandle,
    peer_addresses: Vec<SocketAddr>,
) {
    // Create the connections, we don't want to use them, we just need to open the NAT port.
    peer_addresses.into_iter().for_each(|a| {
        new_con_handle.new_connection(a);
    });
}

/// `BuildConnectionToPeerRemote` is used by the remote side of the new connection.
pub struct BuildConnectionToPeerRemote {
    stream: Option<ProtocolStream<BuildConnectionToPeerProtocol>>,
    internet_addr: Option<SocketAddr>,
    peer_identifier: PubKeyHash,
    new_stream_handle: NewStreamHandle,
    pass_stream_to_context: PassStreamToContext,
    new_con_handle: NewConnectionHandle,
}

impl BuildConnectionToPeerRemote {
    pub fn new(
        stream: strategies::Stream,
        peer_identifier: PubKeyHash,
        new_stream_handle: NewStreamHandle,
        pass_stream_to_context: PassStreamToContext,
        new_con_handle: NewConnectionHandle,
    ) -> BuildConnectionToPeerRemote {
        BuildConnectionToPeerRemote {
            stream: Some(stream.into()),
            peer_identifier,
            new_stream_handle,
            pass_stream_to_context,
            new_con_handle,
            internet_addr: None,
        }
    }
}

impl Future for BuildConnectionToPeerRemote {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match try_ready!(
                self.stream
                    .as_mut()
                    .expect("Can not be polled twice!")
                    .poll()
            ) {
                Some(BuildConnectionToPeerProtocol::InternetAddressInformation(addr)) => {
                    self.internet_addr = Some(addr);
                }
                Some(BuildConnectionToPeerProtocol::ExchangeAddressInformation(
                    remote_addresses,
                )) => {
                    let mut addresses = get_interface_addresses(
                        self.stream
                            .as_ref()
                            .unwrap()
                            .get_ref()
                            .get_ref()
                            .get_ref()
                            .local_addr(),
                    );
                    addresses.push(
                        self.internet_addr
                            .expect("Internet address should already be set!"),
                    );

                    self.stream.as_mut().unwrap().start_send(
                        BuildConnectionToPeerProtocol::ExchangeAddressInformation(addresses),
                    )?;
                    self.stream.as_mut().unwrap().poll_complete()?;

                    create_poke_connections(&mut self.new_con_handle, remote_addresses);
                }
                Some(BuildConnectionToPeerProtocol::ProxyConnection) => {
                    self.pass_stream_to_context.pass_stream(
                        self.stream.take().unwrap().into(),
                        self.peer_identifier.clone(),
                        self.new_stream_handle.clone(),
                        true,
                    );

                    return Ok(Ready(()));
                }
                None => {
                    return Ok(Ready(()));
                }
            }
        }
    }
}

fn prepare_stream_for_building(
    peer: ProtocolStream<BuildConnectionToPeerProtocol>,
) -> Result<strategies::Stream> {
    let peer_addr = peer.get_ref().get_ref().get_ref().peer_addr();
    let mut peer = peer;
    peer.start_send(BuildConnectionToPeerProtocol::InternetAddressInformation(
        peer_addr,
    ))?;
    peer.poll_complete()?;
    Ok(peer.into())
}

pub fn prepare_streams_for_building<T, R>(
    peer: T,
    requested_peer: R,
) -> Result<(strategies::Stream, strategies::Stream)>
where
    T: Into<ProtocolStream<BuildConnectionToPeerProtocol>>,
    R: Into<ProtocolStream<BuildConnectionToPeerProtocol>>,
{
    let peer = prepare_stream_for_building(peer.into())?;
    let requested_peer = prepare_stream_for_building(requested_peer.into())?;
    Ok((peer, requested_peer))
}
