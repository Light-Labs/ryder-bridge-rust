/// A server for serial port communication.

use serialport::{ClearBuffer, Error, SerialPort};
use tokio::sync::watch::{self, Receiver, Sender};
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use std::{io, thread};
use std::time::Duration;

use super::{Client, Data, DeviceState};

/// A server that directly communicates with a serial port, forwarding data to and from a
/// [`Client`] and handling serial device disconnections and reconnections.
pub struct Server {
    /// The path to the serial port.
    path: String,
    /// The serial port itself if it is currently opened.
    port: Option<Box<dyn SerialPort>>,
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
    /// A receiver for ctrl-c signals.
    ctrlc_rx: Receiver<()>,
}

impl Server {
    /// Returns a new `Server`, [`Client`], and an error if the serial port could not be opened.
    ///
    /// The server will continue to retry opening the serial port even if it fails initially.
    pub fn new(path: String, ctrlc_rx: Receiver<()>) -> (Self, Client, Option<Error>) {
        // Try to open the serial port
        let port = open_serial_port(&path);

        let initial_state = match port {
            Ok(_) => DeviceState::Connnected,
            Err(_) => DeviceState::NotConnected,
        };

        let (port, error) = match port {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e)),
        };

        // Open the device state channel
        let (device_state_tx, device_state_rx) = watch::channel(initial_state);
        // Open the serial port write channel
        let (write_tx, write_rx) = mpsc::unbounded();
        // Open the serial port read channel
        let (read_tx, read_rx) = mpsc::unbounded();

        let server = Server {
            path,
            port,
            device_state: device_state_tx,
            read_buf: vec![0; 256],
            data_to_write: None,
            rx: write_rx,
            tx: read_tx,
            ctrlc_rx,
        };

        let client = Client::new(write_tx, read_rx, device_state_rx);

        (server, client, error)
    }

    /// Runs the serial port communication server loop.
    ///
    /// A separate thread must be used for this as it runs in an infinite loop.
    pub fn run(mut self) {
        loop {
            // Watch for exit signal
            if self.ctrlc_rx.has_changed().unwrap_or(true) {
                break;
            }

            if self.port.is_some() {
                // If the port is open, process I/O
                if let Err(e) = self.process_io() {
                    // If the port could not be accessed, the device was probably disconnected, so the
                    // port should be closed
                    println!("Device disconnected: {}", e);
                    self.port = None;
                    self.device_state.send(DeviceState::NotConnected).unwrap();
                }
            } else {
                // Otherwise, keep retrying to re-open it
                match open_serial_port(&self.path) {
                    Ok(p) => {
                        println!("Device reconnected");
                        self.port = Some(p);
                        self.device_state.send(DeviceState::Connnected).unwrap();
                    }
                    // Don't retry too often
                    Err(_) => thread::sleep(Duration::from_secs(2)),
                }
            }
        }
    }

    /// Processes serial port and [`Client`] I/O. Returns `Err` if the serial port could not be
    /// accessed.
    fn process_io(&mut self) -> Result<(), Error> {
        // This function will never be called while the port is closed
        let port = self.port.as_mut().unwrap();

        // Write data to port (prioritizing data that previously failed to write)
        if self.data_to_write.is_none() {
            // The unwrap here will fail if the channel is closed, but the server should always
            // be closed by ctrl-c before the client (which owns the tx), so it shouldn't be an
            // issue
            self.data_to_write = self.rx.try_next().map(Option::unwrap).ok();
        }

        if let Some(ref mut d) = self.data_to_write {
            let res = port.write(d);

            match res {
                Ok(bytes) => {
                    // Check if not all bytes were written
                    if bytes < d.len() {
                        d.truncate(d.len() - bytes);
                    } else {
                        self.data_to_write = None;
                    }
                }
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
        match port.read(&mut self.read_buf) {
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

/// Attempts to open the serial port at the provided path.
fn open_serial_port(path: &str) -> Result<Box<dyn SerialPort>, Error> {
    // The baud rate must be set to 0 on macOS to avoid a bug
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let baud_rate = 0;
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let baud_rate = 115_200;

    let timeout = Duration::from_millis(10);

    serialport::new(path, baud_rate)
        .timeout(timeout)
        .open()
        .and_then(|p| {
            // Clear the serial port buffers to avoid reading garbage data
            p.clear(ClearBuffer::All).map(|_| p)
        })
}
