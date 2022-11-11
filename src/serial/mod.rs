//! Serial port communication and handling.

mod client;
mod server;

pub use client::Client;
pub use server::Server;

/// The message type used in channels related to the serial port.
pub type Data = Vec<u8>;

/// The state of the serial device connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    /// The connection is active.
    Connnected,
    /// The connection is not active.
    NotConnected,
}
