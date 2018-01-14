extern crate bytes;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate futures;
extern crate hole_punch;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;
extern crate tokio_file_unix;
#[macro_use]
extern crate tokio_io;

use hole_punch::dev_client::{Client, NewService, Service, ServiceControl, ServiceInformEvent};
use hole_punch::errors::*;

use tokio_core::reactor::Core;

use tokio_io::AsyncRead;

use std::net::SocketAddr;
use std::io::{self, Write};
use std::cell::RefCell;
use std::env;

use futures::{future, Future, IntoFuture, Poll, Sink, Stream, stream};

#[derive(Deserialize, Serialize, Clone)]
enum CarrierProtocol {
    Register { name: String },
    Registered,
    RequestDevice { name: String },
    DeviceNotFound,
    AlreadyConnected,
}

struct CarrierService {
    control: ServiceControl<CarrierProtocol>,
}

impl Service for CarrierService {
    type Message = CarrierProtocol;

    fn on_message(&mut self, msg: &Self::Message) -> Result<Option<Self::Message>> {
        match msg {
            &CarrierProtocol::Registered => {
                eprintln!("REGISTERED");
                Ok(None)
            }
            &CarrierProtocol::Register { ref name } => {
                eprintln!("REGISTER: {}", name);
                Ok(None)
            }
            &CarrierProtocol::DeviceNotFound => {
                eprintln!("DEVICENOTFOUND");
                bail!("NOT FOUND");
            }
            &CarrierProtocol::AlreadyConnected => {
                eprintln!("ALREADY");
                self.control.use_as_result();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn inform(&mut self, evt: ServiceInformEvent) {
        match evt {
            ServiceInformEvent::Connecting => eprintln!("CONNECTING"),
        }
    }
}

struct NewCarrierService {}

impl NewService<CarrierProtocol> for NewCarrierService {
    type Service = CarrierService;

    fn new_service(
        &mut self,
        mut control: ServiceControl<CarrierProtocol>,
        addr: SocketAddr,
    ) -> Self::Service {
        eprintln!("new connection to: {}", addr);

        control.send_message(CarrierProtocol::RequestDevice {
            name: "nice".to_owned(),
        });
        CarrierService { control }
    }
}

struct TestReader<R: AsyncRead> {
    read: R,
    sink: stream::SplitSink<hole_punch::PureConnection>,
    buf: Vec<u8>,
}

impl<R: AsyncRead> Future for TestReader<R> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let len = try_nb!(self.read.read(&mut self.buf));

            if len > 0 {
                self.sink.start_send(bytes::BytesMut::from(&self.buf[..len]));
                self.sink.poll_complete();
            }
        }
    }
}

fn main() {
    env_logger::init();
    // let device = env::args().nth(1).expect("please give name for device!");
    let server_addr = ([176, 9, 73, 99], 22222).into();
    // let server_addr = ([127, 0, 0, 1], 22222).into();
    let mut evt_loop = Core::new().unwrap();

    let new_service = NewCarrierService {};

    let mut client = Client::new(evt_loop.handle(), new_service, true).expect("client");
    let addr: SocketAddr = server_addr;
    client.connect_to(&addr).expect("connect");

    let con = evt_loop
        .run(client.into_future().map_err(|_| ()))
        .unwrap()
        .0
        .unwrap();

    let handle = evt_loop.handle();

    let stdin_orig = std::io::stdin();
    let stdin = tokio_file_unix::StdFile(stdin_orig.lock());
    let stdin = tokio_file_unix::File::new_nb(stdin).unwrap();
    let stdin = stdin.into_reader(&handle).unwrap();

    let (send, recv) = <hole_punch::PureConnection as Stream>::split(con);

    handle.spawn(recv.for_each(|buf| {
        std::io::stdout().write(&buf);
        std::io::stdout().flush();
        Ok(())
    }).map_err(|_| ()));

    let buf: Vec<u8> = vec![0; 1024];
    evt_loop.run(TestReader { read: stdin, buf, sink: send });
}
