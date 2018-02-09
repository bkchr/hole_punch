use std::io;
use serde_json;
use picoquic;

error_chain! {
    foreign_links {
        Io(io::Error);
        Json(serde_json::Error);
        PicoQuic(picoquic::Error);
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{:?}", err))
    }
}
