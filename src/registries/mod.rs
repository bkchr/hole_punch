mod mdns;
mod remote;

pub use self::mdns::MdnsRegistry;
pub use remote::{IncomingStream as RemoteRegistryIncomingStream, RemoteRegistry, Resolve};
