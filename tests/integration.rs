use hole_punch::{
    Config, Context, CreateConnectionToPeerHandle, Error, FileFormat, ProtocolStream, PubKeyHash,
    SendFuture,
};

use tokio::{
    prelude::FutureExt,
    runtime::{Runtime, TaskExecutor},
    timer::Interval,
};

use serde_derive::{Deserialize, Serialize};

use futures::{sync::oneshot, Future, Sink, Stream as FStream};

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use pretty_env_logger;

use log::{error, info, LevelFilter};

const PEER0_KEY: &[u8] = include_bytes!("certs/peer0_key.pem");
const PEER0_CERT: &[u8] = include_bytes!("certs/peer0_cert.pem");

const PEER1_KEY: &[u8] = include_bytes!("certs/peer1_key.pem");
const PEER1_CERT: &[u8] = include_bytes!("certs/peer1_cert.pem");

const PEER2_KEY: &[u8] = include_bytes!("certs/peer2_key.pem");
const PEER2_CERT: &[u8] = include_bytes!("certs/peer2_cert.pem");

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum TestProtocol {
    Hello(PubKeyHash),
}

type TestProtocolStream = ProtocolStream<TestProtocol>;

fn init_log() {
    pretty_env_logger::formatted_builder()
        .filter_module("hole_punch", LevelFilter::Debug)
        .try_init()
        .ok();
}

fn start_peer(
    key: &[u8],
    cert: &[u8],
    remote_peer: Option<SocketAddr>,
    mdns: Option<&'static str>,
    executor: TaskExecutor,
) -> Context {
    let config_builder = Config::builder()
        .set_private_key(key.into(), FileFormat::PEM)
        .set_certificate_chain(vec![cert.into()], FileFormat::PEM);

    let config_builder = if let Some(remote) = remote_peer {
        config_builder.add_remote_peer(remote)
    } else {
        config_builder
    };

    let config = if let Some(service) = mdns {
        config_builder.enable_mdns(service).build()
    } else {
        config_builder.build()
    }
    .expect("Creates Config.");

    Context::new(
        PubKeyHash::from_x509_pem(cert, false).expect("Creates PubKeyHash from x509 cert."),
        executor,
        config,
    )
    .expect("Creates Context")
}

fn start_peer0(remote_peer: Option<SocketAddr>, executor: TaskExecutor) -> Context {
    let peer = start_peer(PEER0_KEY, PEER0_CERT, remote_peer, None, executor);
    info!("Started peer0 {}", peer.local_peer_identifier());
    peer
}

fn start_peer0_with_mdns(executor: TaskExecutor) -> Context {
    let peer = start_peer(
        PEER0_KEY,
        PEER0_CERT,
        None,
        Some("hole_punch_test"),
        executor,
    );
    info!("Started peer0 with mDNS {}", peer.local_peer_identifier());
    peer
}

fn start_peer1(remote_peer: Option<SocketAddr>, executor: TaskExecutor) -> Context {
    let peer = start_peer(PEER1_KEY, PEER1_CERT, remote_peer, None, executor);
    info!("Started peer1 {}", peer.local_peer_identifier());
    peer
}

fn start_peer1_with_mdns(executor: TaskExecutor) -> Context {
    let peer = start_peer(
        PEER1_KEY,
        PEER1_CERT,
        None,
        Some("hole_punch_test"),
        executor,
    );
    info!("Started peer1 with mDNS {}", peer.local_peer_identifier());
    peer
}

fn start_peer2(remote_peer: Option<SocketAddr>, executor: TaskExecutor) -> Context {
    let peer = start_peer(PEER2_KEY, PEER2_CERT, remote_peer, None, executor);
    info!("Started peer2 {}", peer.local_peer_identifier());
    peer
}

fn get_peer_address(peer: &Context) -> SocketAddr {
    ([127, 0, 0, 1], peer.quic_local_addr().port()).into()
}

fn spawn_hello_responder(peer: Context, executor: TaskExecutor) {
    let peer_identifier = peer.local_peer_identifier().clone();
    executor.spawn(
        peer.for_each(move |s| {
            let stream: TestProtocolStream = s.into();
            tokio::spawn(
                stream
                    .send(TestProtocol::Hello(peer_identifier.clone()))
                    .map(|_| ())
                    .map_err(|_| ()),
            );
            Ok(())
        })
        .map_err(|_| ()),
    );
}

/// Connect to the given peer and wait for the `Hello` message.
///
/// Returns a `Future` that will resolve to `true` when everything worked as expected.
fn connect_to_peer_and_recv_hello_message(
    handle: CreateConnectionToPeerHandle,
    remote_peer_identifier: PubKeyHash,
) -> impl SendFuture<Item = bool, Error = Error> {
    handle
        .create_connection_to_peer(remote_peer_identifier.clone())
        .and_then(|con| {
            let con: TestProtocolStream = con.into();
            con.into_future().map(|v| v.0).map_err(|e| Error::from(e.0))
        })
        .map(move |msg| Some(TestProtocol::Hello(remote_peer_identifier)) == msg)
}

fn expect_connect_to_peer_and_recv_hello_message(
    peer: &Context,
    remote_peer_identifier: PubKeyHash,
    runtime: &mut Runtime,
) {
    assert!(
        runtime
            .block_on(connect_to_peer_and_recv_hello_message(
                peer.create_connection_to_peer_handle(),
                remote_peer_identifier
            ))
            .unwrap(),
        "Did not connect to requested peer or did not receive the correct hello message"
    );
}

#[test]
fn peer1_connects_to_peer0() {
    init_log();
    let mut runtime = Runtime::new().expect("Creates runtime");

    let peer0 = start_peer0(None, runtime.executor());
    let peer1 = start_peer1(Some(get_peer_address(&peer0)), runtime.executor());

    let peer0_identifier = peer0.local_peer_identifier().clone();
    spawn_hello_responder(peer0, runtime.executor());

    expect_connect_to_peer_and_recv_hello_message(&peer1, peer0_identifier, &mut runtime);
}

#[test]
fn peer1_connects_to_peer0_with_mdns() {
    init_log();
    let mut runtime = Runtime::new().expect("Creates runtime");

    let peer0 = start_peer0_with_mdns(runtime.executor());
    let peer1 = start_peer1_with_mdns(runtime.executor());

    let peer0_identifier = peer0.local_peer_identifier().clone();
    spawn_hello_responder(peer0, runtime.executor());

    let (sender, receiver) = oneshot::channel();
    let mut sender = Some(sender);
    let handle = peer1.create_connection_to_peer_handle();
    let _ = runtime
        .block_on(
            Interval::new(Instant::now(), Duration::from_millis(500))
                .map_err(Error::from)
                .and_then(move |_| {
                    connect_to_peer_and_recv_hello_message(handle.clone(), peer0_identifier.clone())
                        .then(|res| Ok(res.ok()))
                })
                .for_each(move |res| {
                    if res.unwrap_or(false) {
                        if let Some(sender) = sender.take() {
                            let _ = sender.send(());
                        }
                    }

                    Ok(())
                })
                .map_err(|e| error!("{:?}", e))
                .select(receiver.timeout(Duration::from_secs(60)).map_err(|_| ()))
                .map_err(|_| ()),
        )
        .expect("Finds peer with mDNS and connects.");
}

#[test]
fn peer1_connects_to_peer2_via_peer0() {
    init_log();
    let mut runtime = Runtime::new().expect("Creates runtime");

    let peer0 = start_peer0(None, runtime.executor());
    let peer1 = start_peer1(Some(get_peer_address(&peer0)), runtime.executor());
    let peer2 = start_peer2(Some(get_peer_address(&peer0)), runtime.executor());

    // By requesting a none existing peer, we make sure that `peer2` is know at `peer0`
    let no_peer = PubKeyHash::from_hashed(&[0, 0, 0]).unwrap();
    assert_eq!(
        runtime
            .block_on(peer2.create_connection_to_peer(no_peer.clone()))
            .err()
            .unwrap(),
        Error::PeerNotFound(no_peer)
    );

    let peer2_identifier = peer2.local_peer_identifier().clone();
    spawn_hello_responder(peer2, runtime.executor());

    expect_connect_to_peer_and_recv_hello_message(&peer1, peer2_identifier, &mut runtime);
}

#[test]
fn dropping_context_stops_stream() {
    init_log();
    let mut runtime = Runtime::new().expect("Creates runtime");

    let peer0 = start_peer0(None, runtime.executor());
    let peer1 = start_peer1(Some(get_peer_address(&peer0)), runtime.executor());

    let peer0_identifier = peer0.local_peer_identifier().clone();
    runtime.spawn(
        peer0
            .for_each(|s| {
                tokio::spawn(s.into_future().map(|_| ()).map_err(|_| ()));
                Ok(())
            })
            .map_err(|_| ()),
    );

    let con: TestProtocolStream = runtime
        .block_on(peer1.create_connection_to_peer(peer0_identifier))
        .expect("Connects to peer0")
        .into();

    drop(peer1);

    let res = runtime
        .block_on(con.into_future().map(|v| v.0).map_err(|e| e.0))
        .expect("Connection returns `None`");
    assert_eq!(None, res);
}
