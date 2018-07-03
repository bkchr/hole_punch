use PubKeyHash;

use failure;
pub use failure::ResultExt;

use std::{io, result};

use picoquic;

use serde_json;

use futures;

use bincode;

use openssl;

use hex;

pub type Result<T> = result::Result<T, Error>;

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display = "Picoquic Error {}", _0)]
    Picoquic(#[cause] picoquic::Error),
    #[fail(display = "IO Error {}", _0)]
    Io(#[cause] io::Error),
    #[fail(display = "Json Error {}", _0)]
    Json(#[cause] serde_json::Error),
    #[fail(display = "Channel canceled Error {}", _0)]
    ChannelCanceled(#[cause] futures::Canceled),
    #[fail(display = "Error {}", _0)]
    Custom(failure::Error),
    #[fail(display = "Openssl error {}", _0)]
    Openssl(#[cause] openssl::error::ErrorStack),
    #[fail(display = "Hex error {}", _0)]
    Hex(#[cause] hex::FromHexError),
    #[fail(display = "Peer {} not found.", _0)]
    PeerNotFound(PubKeyHash),
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{:?}", err))
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<picoquic::Error> for Error {
    fn from(err: picoquic::Error) -> Error {
        Error::Picoquic(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Error {
        Error::Json(err)
    }
}

impl From<futures::Canceled> for Error {
    fn from(err: futures::Canceled) -> Error {
        Error::ChannelCanceled(err)
    }
}

impl From<failure::Error> for Error {
    fn from(err: failure::Error) -> Error {
        Error::Custom(err)
    }
}

impl From<bincode::Error> for Error {
    fn from(err: bincode::Error) -> Error {
        Error::Custom(err.into())
    }
}

impl From<openssl::error::ErrorStack> for Error {
    fn from(err: openssl::error::ErrorStack) -> Error {
        Error::Openssl(err.into())
    }
}

impl From<hex::FromHexError> for Error {
    fn from(err: hex::FromHexError) -> Error {
        Error::Hex(err.into())
    }
}

impl From<&'static str> for Error {
    fn from(err: &'static str) -> Error {
        Error::Custom(::failure::err_msg::<&'static str>(err).into())
    }
}

//FIXME: Remove when upstream provides a better bail macro
macro_rules! bail {
    ($e:expr) => {
        return Err(::failure::err_msg::<&'static str>($e).into());
    };
    ($fmt:expr, $($arg:tt)+) => {
        return Err(::failure::err_msg::<String>(format!($fmt, $($arg)+)).into());
    };
}
