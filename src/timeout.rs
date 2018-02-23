use error::*;

use std::time::{Duration, Instant};

use tokio_core::reactor::{self, Handle};

use futures::{Future, Poll};

pub struct Timeout(reactor::Timeout, Duration);

impl Timeout {
    pub fn new(dur: Duration, handle: &Handle) -> Timeout {
        Timeout(
            reactor::Timeout::new(dur, handle).expect("no timeout!!"),
            dur,
        )
    }

    pub fn reset(&mut self) {
        self.0.reset(Instant::now() + self.1);
    }

    pub fn new_reset(mut self) -> Self {
        self.reset();
        self
    }
}

impl Future for Timeout {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.poll());

        // if we come to this point, the timer finished, aka timeout!
        bail!("Timeout")
    }
}
