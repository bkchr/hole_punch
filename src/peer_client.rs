use errors::*;
use udp;
use protocol;

use std::net::SocketAddr;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_serde_json::{ReadJson, WriteJson};

use futures::{Future, Sink, Stream};

pub fn peer_client_main(server: SocketAddr) {
    let mut evt_loop = Core::new().expect("error creating evt loop");
    let handle = evt_loop.handle();

    let registration = udp::connect_async(server, &handle).and_then(|stream| {
        // we work with a length delimited stream
        let length_delimited = length_delimited::Framed::new(stream);
        let (writer, reader) = length_delimited.split();

        let json_writer = WriteJson::new(writer);
        let json_reader = ReadJson::<_, protocol::Registration>::new(reader);

        // send our request and after that, wait for the answer
        json_writer
            .send(protocol::Registration {
                name: "peer_client".to_string(),
            })
            .then(|r| r.chain_err(|| "error sending request"))
            .and_then(|_| {
                json_reader
                    .then(|r| r.chain_err(|| "error receiving response"))
                    .into_future()
                    .map(|v| {
                        println!("{:?}", v.0);
                        v.0
                    })
                    .map_err(|e| e.0)
            })
    });

    let answer = evt_loop
        .run(registration)
        .expect("error registering")
        .unwrap();
    if answer.name != "peer_client" {
        panic!("wrong answer");
    }
}
