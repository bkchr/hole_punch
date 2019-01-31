use hole_punch::{Config, Context, Error, FileFormat, ProtocolStream, PubKeyHash};

use tokio::runtime::{Runtime, TaskExecutor};

use serde_derive::{Deserialize, Serialize};

use futures::{Future, Sink, Stream as FStream};

use std::net::SocketAddr;

use pretty_env_logger;

use log::{info, LevelFilter};

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
        .filter_level(LevelFilter::Debug)
        .try_init()
        .ok();
}

fn start_peer(
    key: &[u8],
    cert: &[u8],
    remote_peer: Option<SocketAddr>,
    executor: TaskExecutor,
) -> Context {
    let config_builder = Config::builder()
        .set_private_key(key.into(), FileFormat::PEM)
        .set_certificate_chain(vec![cert.into()], FileFormat::PEM);

    let config = if let Some(remote) = remote_peer {
        config_builder.add_remote_peer(remote).build()
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
    let peer = start_peer(PEER0_KEY, PEER0_CERT, remote_peer, executor);
    info!("Started peer0 {}", peer.local_peer_identifier());
    peer
}

fn start_peer1(remote_peer: Option<SocketAddr>, executor: TaskExecutor) -> Context {
    let peer = start_peer(PEER1_KEY, PEER1_CERT, remote_peer, executor);
    info!("Started peer1 {}", peer.local_peer_identifier());
    peer
}

fn start_peer2(remote_peer: Option<SocketAddr>, executor: TaskExecutor) -> Context {
    let peer = start_peer(PEER2_KEY, PEER2_CERT, remote_peer, executor);
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

fn expect_connect_to_peer_and_recv_hello_message(
    peer: &Context,
    remote_peer_identifier: PubKeyHash,
    runtime: &mut Runtime,
) {
    let con: TestProtocolStream = runtime
        .block_on(peer.create_connection_to_peer(remote_peer_identifier.clone()))
        .expect("Connects to peer0")
        .into();

    let message = runtime
        .block_on(con.into_future().map(|v| v.0).map_err(|e| e.0))
        .expect("Retrieves hello message.")
        .expect("Is not `None`");

    assert_eq!(TestProtocol::Hello(remote_peer_identifier), message);
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
