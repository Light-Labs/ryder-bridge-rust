//! A client for serial port communication.

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch::Receiver;

use super::{Data, DeviceState};

/// A client of the serial port connection that allows sending data to be written and receiving
/// responses back.
///
/// A single client is provided when the serial port is first opened, and it should be passed around
/// as needed.
pub struct Client {
    /// A sender for data to be written to the serial port.
    pub tx: UnboundedSender<Data>,
    /// A receiver for responses from the serial port.
    pub rx: UnboundedReceiver<Data>,
    /// A receiver for the device state signal.
    pub device_state: Receiver<DeviceState>,
}

impl Client {
    pub fn new(
        tx: UnboundedSender<Data>,
        rx: UnboundedReceiver<Data>,
        device_state: Receiver<DeviceState>,
    ) -> Self {
        Client {
            tx,
            rx,
            device_state,
        }
    }
}
