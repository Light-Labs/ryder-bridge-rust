/// A server for serial port communication.

use serialport::Error;
use tokio::sync::watch::{self, Receiver, Sender};
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use std::io::{self, Read, Write};
use std::thread;
use std::path::PathBuf;
use std::time::Duration;

use super::{Client, Data, DeviceState};
use super::port::Port;

/// A server that directly communicates with a serial port, forwarding data to and from a
/// [`Client`] and handling serial device disconnections and reconnections.
pub struct Server {
    /// The serial port itself.
    port: Port,
    /// Whether the serial port was accessible when last accessed.
    port_accessible: bool,
    /// A sender for notifying the client of serial device state changes (i.e., reconnections and
    /// disconnections).
    device_state: Sender<DeviceState>,
    /// A buffer for reading data from the serial port.
    read_buf: Vec<u8>,
    /// A buffer for storing partially written data so that the write can be completed later.
    data_to_write: Option<Vec<u8>>,
    /// A receiver for data to be written to the serial port.
    rx: UnboundedReceiver<Data>,
    /// A sender for data received from the serial port.
    tx: UnboundedSender<Data>,
    /// A receiver for termination signals.
    terminate_rx: Receiver<()>,
}

impl Server {
    /// Returns a new `Server`, [`Client`], and an error if the serial port could not be opened.
    ///
    /// The server will continue to retry opening the serial port even if it fails initially.
    /// `terminate_rx` is watched for a signal that the bridge is shutting down, in which case the
    /// serial port and the server are closed.
    pub fn new(path: PathBuf, terminate_rx: Receiver<()>) -> (Self, Client, Result<(), Error>) {
        // Try to open the serial port
        let (port, error) = Port::new(path);

        let initial_state = if error.is_ok() {
            DeviceState::Connnected
        } else {
            DeviceState::NotConnected
        };

        // Open the device state channel
        let (device_state_tx, device_state_rx) = watch::channel(initial_state);
        // Open the serial port write channel
        let (write_tx, write_rx) = mpsc::unbounded();
        // Open the serial port read channel
        let (read_tx, read_rx) = mpsc::unbounded();

        let server = Server {
            port,
            port_accessible: error.is_ok(),
            device_state: device_state_tx,
            read_buf: vec![0; 256],
            data_to_write: None,
            rx: write_rx,
            tx: read_tx,
            terminate_rx,
        };

        let client = Client::new(write_tx, read_rx, device_state_rx);

        (server, client, error)
    }

    /// Runs the serial port communication server loop.
    ///
    /// A separate thread must be used for this as it runs in an infinite loop.
    pub fn run(mut self) {
        loop {
            // Watch for termination signal
            if self.terminate_rx.has_changed().unwrap_or(true) {
                break;
            }

            if self.port_accessible {
                // If the port is accessible, process I/O
                if let Err(e) = self.process_io() {
                    self.on_device_disconnected(e);
                }
            } else {
                // Otherwise, keep retrying to re-open it
                match self.port.try_open() {
                    Ok(_) => self.on_device_connected(),
                    // Don't retry too often
                    Err(_) => thread::sleep(Duration::from_secs(2)),
                }
            }
        }
    }

    /// Updates the port state flag and notifies the client that the serial device disconnected.
    fn on_device_disconnected(&mut self, error: Error) {
        println!("Device disconnected: {}", error);
        self.port_accessible = false;
        self.device_state.send(DeviceState::NotConnected).unwrap();
    }

    /// Updates the port state flag and notifies the client that the serial device connected or
    /// reconnected.
    fn on_device_connected(&mut self) {
        println!("Device connected");
        self.port_accessible = true;
        self.device_state.send(DeviceState::Connnected).unwrap();
    }

    /// Processes serial port and [`Client`] I/O. Returns `Err` if the serial port could not be
    /// accessed.
    fn process_io(&mut self) -> Result<(), Error> {
        // Write data to port (prioritizing data that previously failed to write)
        if self.data_to_write.is_none() {
            // The unwrap here will fail if the channel is closed, but the server should always
            // be terminated before the client (which owns the tx), so it shouldn't be an issue
            self.data_to_write = self.rx.try_next().map(Option::unwrap).ok();
        }

        if let Some(ref mut d) = self.data_to_write {
            let res = write(&mut self.port, d);

            match res {
                Ok(remaining) => self.data_to_write = remaining,
                Err(e) => {
                    match e.kind() {
                        // Ignore temporary write failures
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::Interrupted
                            | io::ErrorKind::TimedOut => {},
                        _ => return Err(e.into()),
                    }
                }
            }
        }

        // Read data from port as it's received
        match self.port.read(&mut self.read_buf) {
            Ok(bytes) => self.tx.unbounded_send(self.read_buf[..bytes].to_vec()).unwrap(),
            Err(e) => match e.kind() {
                // Ignore temporary read failures
                io::ErrorKind::WouldBlock
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::TimedOut => {},
                _ => return Err(e.into()),
            }
        }

        Ok(())
    }
}

/// Writes `data` to `out`. Returns `Ok(None)` if all the data was successfully written, or
/// `Ok(Some)` with the remaining data otherwise.
fn write<F: Write>(mut out: F, data: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
    let bytes = out.write(data)?;

    if bytes < data.len() {
        Ok(Some(data[bytes..].to_vec()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write() {
        let mut buf = vec![0; 4];
        let data = vec![1, 2, 3, 4];
        // All the data was written
        assert_eq!(None, write(&mut buf[..], &data).unwrap());

        let mut buf = vec![0; 3];
        let data = vec![1, 2, 3, 4];
        // Remaining data is returned
        assert_eq!(Some(vec![4]), write(&mut buf[..], &data).unwrap());
    }
}
