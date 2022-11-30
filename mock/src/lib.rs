//! Mock types for testing.

pub mod serial;

use std::env;
use std::net::SocketAddr;

/// Returns the listening address to use for bridge tests. The port is controllable with the
/// `RYDER_BRIDGE_TEST_PORT` environment variable .
pub fn get_bridge_test_addr() -> SocketAddr {
    let port = if let Ok(Ok(p)) = env::var("RYDER_BRIDGE_TEST_PORT").map(|s| s.parse()) {
        p
    } else {
        8080
    };

    format!("127.0.0.1:{}", port).parse().unwrap()
}
