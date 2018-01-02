use std::io;
use tokio_serde_json;

error_chain! {
    foreign_links {
        Io(io::Error);
        TJson(tokio_serde_json::Error);
    }
}
