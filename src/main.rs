//! See `README.md` and `lib.rs` for documentation.

use std::env;
use std::net::SocketAddr;
use std::str::FromStr;

#[tokio::main]
async fn main() {
    let mut args = env::args();

    let ryder_port = args.nth(1).expect("Ryder port is required");
    let addr = args.next().expect("Listening address is required");

    ryder_bridge::launch(
        SocketAddr::from_str(&addr).expect("Invalid listening address"),
        ryder_port,
    ).await;
}
