use errors::*;
use strategies;
use config::Config;

use std::marker::PhantomData;

use futures::stream::{futures_unordered, FuturesUnordered, StreamFuture};
use futures::Stream;

use tokio_core::reactor::Handle;

pub struct Context<P> {
    strategies: FuturesUnordered<StreamFuture<strategies::Strategy>>,
    _marker: PhantomData<P>,
}

impl<P> Context<P> {
    fn new(handle: Handle, config: Config) -> Result<Context<P>> {
        let strats =
            strategies::init(handle, &config).chain_err(|| "error initializing the strategies")?;

        Ok(Context {
            strategies: futures_unordered(strats.into_iter().map(|s| s.into_future())),
        })
    }
}

impl<P> Stream for Context<P> {
    type Item = Connection<P>;
    type Error = Error;
}

struct Connection<P> {
    
}
