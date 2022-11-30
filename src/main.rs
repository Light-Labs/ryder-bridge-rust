//! See `README.md` and `lib.rs` for documentation.

use clap::Parser;

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version)]
#[command(about = "A bridge that facilitates communication between Ryder devices and applications.")]
struct Cli {
    #[arg(name = "serial port path")]
    serial_port: PathBuf,
    #[arg(name = "listening address")]
    addr: SocketAddr,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Launch the bridge
    let (task_handle, _) = ryder_bridge::launch(cli.addr, cli.serial_port);

    // Wait for it to close
    task_handle.await.unwrap();
}
