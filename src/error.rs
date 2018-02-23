use failure;
pub use failure::ResultExt;

use std::io;
use std::result;

use picoquic;

use serde_json;

use futures;

pub type Result<T> = result::Result<T, Error>;

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display = "Picoquic Error {}", _0)]
    Picoquic(#[cause] picoquic::Error),
    #[fail(display = "IO Error {}", _0)]
    IoError(#[cause] io::Error),
    #[fail(display = "Json Error {}", _0)]
    Json(#[cause] serde_json::Error),
    #[fail(display = "Channel canceled Error {}", _0)]
    ChannelCanceled(#[cause] futures::Canceled),
    #[fail(display = "Error {}", _0)]
    Custom(failure::Error),
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{:?}", err))
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::IoError(err)
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

//FIXME: Remove when upstream provides a better bail macro
macro_rules! bail {
    ($e:expr) => {
        return Err(::failure::err_msg::<&'static str>($e).into());
    };
    ($fmt:expr, $($arg:tt)+) => {
        return Err(::failure::err_msg::<String>(format!($fmt, $($arg)+)).into());
    };
}
