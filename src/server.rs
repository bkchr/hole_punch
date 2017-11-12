use errors::*;
use udp;
use protocol;

use tokio_core::reactor::Core;
use tokio_io::codec::length_delimited;
use tokio_serde_json::{ReadJson, WriteJson};

use futures::{Future, Sink, Stream};

pub fn server_main() {
    let mut evt_loop = Core::new().expect("could not initialize event loop");

    let evt_loop_handle = evt_loop.handle();
    let evt_loop_handle3 = evt_loop.handle();

    // listen for all incoming requests
    let server =
        udp::accept_async(([0, 0, 0, 0], 22222).into(), &evt_loop_handle3, 4).and_then(|server| {
            server
                .for_each(|(con, addr)| {
                    let length_delimited = length_delimited::Framed::new(con);

                    let (writer, reader) = length_delimited.split();

                    let send = WriteJson::new(writer);
                    let recv = ReadJson::<_, protocol::Registration>::new(reader);

                    let evt_loop_handle2 = evt_loop_handle.clone();
                    evt_loop_handle.spawn(
                        recv.into_future()
                            .map(move |v| {
                                println!("Registered {:?}", v.0);
                                evt_loop_handle2
                                    .spawn(send.send(v.0.unwrap()).map_err(|_| ()).map(|_| ()));
                                ()
                            })
                            .map_err(|_| ())
                            .map(|_| ()),
                    );

                    Ok(())
                })
                .then(|r| r.chain_err(|| "error"))
        });

    evt_loop.run(server).expect("error running the event loop");
}
