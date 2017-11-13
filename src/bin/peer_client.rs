extern crate hole_punch;

fn main() {
    hole_punch::peer_client::peer_client_main(([176, 9, 73, 99], 22222).into());
}
