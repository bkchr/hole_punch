extern crate hole_punch;

pub fn main() {
    hole_punch::peer_client::peer_client_main(([127, 0, 0, 1], 22222).into());
}
