use std::io;
use serde_json;
use picoquic;
use futures;

error_chain! {
    foreign_links {
        Io(io::Error);
        Json(serde_json::Error);
        ChannelCanceled(futures::Canceled);
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{:?}", err))
    }
}

impl From<picoquic::Error> for Error {
    fn from(err: picoquic::Error) -> Error {
        format!("{:?}", err).into()
    }
}
