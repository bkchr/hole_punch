use error::*;

use std::time::{Duration, Instant};

use tokio::timer::Delay;

use futures::{Future, Poll};

pub struct Timeout(Delay, Duration);

impl Timeout {
    pub fn new(dur: Duration) -> Timeout {
        Timeout(
            Delay::new(Instant::now() + dur),
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
