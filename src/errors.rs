use std::io;

use tokio_serde_bincode;

error_chain! {
    foreign_links {
        Io(io::Error);
    }
}
