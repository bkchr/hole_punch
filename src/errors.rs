use std::io;
use serde_json;

error_chain! {
    foreign_links {
        Io(io::Error);
        Json(serde_json::Error);
    }
}
