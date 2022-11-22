//! Abstractions for working with serial ports.

use serialport::{ClearBuffer, Error, ErrorKind, SerialPort, FlowControl};

use std::io::{self, Write, Read};
use std::time::Duration;

/// A function for opening serial ports given a path.
pub type OpenPortFn = Box<dyn Fn(&str) -> serialport::Result<Box<dyn SerialPort>> + Send>;

/// A wrapper around a serial port that can be reopened if the serial port is closed.
pub struct Port {
    /// The function to use when opening the serial port.
    open: OpenPortFn,
    /// The path to the serial port.
    path: String,
    /// The internal serial port, if it is opened.
    port: Option<Box<dyn SerialPort>>,
    /// Whether a device is connected to the other end of the serial port. Always false if the
    /// serial port is not open.
    device_connected: bool,
}

impl Port {
    /// Creates a new `Port` that accesses a serial port at `path`.
    ///
    /// Returns a `Port` regardless of whether the serial port was successfully opened, and
    /// additionally an `Err` if there was an error during the attempt.
    pub fn new(path: String) -> (Self, serialport::Result<()>) {
        Port::new_with_open_fn(path, Box::new(open_serial_port))
    }

    /// Like [`new`][Self::new], but uses a custom function for opening the serial port.
    fn new_with_open_fn(path: String, open_fn: OpenPortFn) -> (Self, serialport::Result<()>) {
        let open_fn = open_fn.into();
        let mut port = Port {
            open: open_fn,
            path,
            port: None,
            device_connected: false,
        };

        let error = port.try_open();

        (port, error)
    }

    /// Returns whether the serial port is accessible (i.e., is open and has a device connected).
    ///
    /// Note that the return value may be out of date; writing to/reading from the port is the only
    /// way to guarantee that the port is currently accessible.
    fn is_accessible(&self) -> bool {
        self.port.is_some() && self.device_connected
    }

    /// Updates the device connected flag. Closes the port and returns `Err` if the serial port is
    /// open but cannot be accessed.
    fn update_port_state(&mut self) -> serialport::Result<()> {
        match self.port {
            // If the port is open, check if a device is connected and update the flag
            Some(ref mut p) => {
                match is_device_connected(p.as_mut()) {
                    Ok(c) => self.device_connected = c,
                    Err(e) => {
                        self.device_connected = false;
                        self.port = None;
                        return Err(e);
                    }
                }
            }
            // If the port is not open, set the flag to false
            None => self.device_connected = false,
        }

        Ok(())
    }

    /// Tries to open the serial port. Returns `Err` if this fails or if the serial port is open
    /// but has no device connected. Returns `Ok` if the serial port is now available.
    ///
    /// If the serial port is already open, this function will only check whether a device is
    /// connected to it.
    pub fn try_open(&mut self) -> serialport::Result<()> {
        // To possibly be returned later
        let not_connected_error = Error::new(
            ErrorKind::NoDevice,
            "The serial port is open but no device is connected",
        );

        if self.port.is_some() {
            // If the port is already open, check if a device is connected
            match self.update_port_state() {
                Ok(()) => {
                    if self.device_connected {
                        // If one is, do nothing
                        Ok(())
                    } else {
                        // If there is not, return an error
                        Err(not_connected_error)
                    }
                },
                // If there was an error while checking, return it
                Err(e) => Err(e),
            }
        } else {
            // If the port is not open, try to open it
            let (port, error) = match (self.open)(&self.path) {
                Ok(p) => (Some(p), Ok(())),
                Err(e) => (None, Err(e)),
            };

            self.port = port;
            // Check whether a device is connected
            self.update_port_state()?;

            // Return an error if the port was successfully opened but a device is not connected to
            // it
            if self.port.is_some() && !self.device_connected {
                Err(not_connected_error)
            } else {
                error
            }
        }
    }

    /// Calls `f` on the internal serial port if it is accessible, or returns `Err` otherwise.
    fn map_port<F, G>(&mut self, f: F) -> io::Result<G>
    where
        F: FnOnce(&mut dyn SerialPort) -> io::Result<G>
    {
        // Check whether the port is accessible
        self.update_port_state()?;

        if self.is_accessible() {
            f(self.port.as_mut().unwrap().as_mut())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
}

impl Read for Port {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.map_port(|p| p.read(buf))
    }
}

impl Write for Port {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.map_port(|p| p.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.map_port(|p| p.flush())
    }
}

/// Attempts to open the serial port at the provided path.
fn open_serial_port(path: &str) -> serialport::Result<Box<dyn SerialPort>> {
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

/// Returns whether a device is connected to the serial port, or `Err` if it could not be accessed.
fn is_device_connected<P: SerialPort + ?Sized>(port: &mut P) -> serialport::Result<bool> {
    match port.flow_control()? {
        // No flow control is used on unix-like systems, but the port must still be accessed in some
        // way to detect errors
        FlowControl::None => port.bytes_to_read().map(|_| true),
        // If RTS/CTS or XON/XOFF are available, use them
        FlowControl::Hardware | FlowControl::Software => port.read_clear_to_send(),
    }
}

#[cfg(test)]
mod tests {
    use serialport::{DataBits, Parity, StopBits};

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const FAKE_PORT: &str = "fakeport";

    // A dummy serial port implementation for testing
    #[derive(Clone)]
    struct TestPort {
        // Determines whether the port is currently accessible
        cts: Arc<AtomicBool>,
        // Whether the port has an error and must close
        error: Arc<AtomicBool>,
    }

    impl TestPort {
        fn new(cts: Arc<AtomicBool>, error: Arc<AtomicBool>) -> serialport::Result<Self> {
            Ok(TestPort {
                cts,
                error,
            })
        }

        // Returns an error if the `error` flag is true
        fn access(&self) -> io::Result<()> {
            if self.error.load(Ordering::SeqCst) {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }

    impl Write for TestPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.access().map(|_| buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.access()
        }
    }

    impl Read for TestPort {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            self.access().map(|_| 0)
        }
    }

    impl SerialPort for TestPort {
        fn name(&self) -> Option<String> {
            None
        }

        fn baud_rate(&self) -> serialport::Result<u32> {
            self.access().map(|_| 115_200).map_err(Into::into)
        }

        fn data_bits(&self) -> serialport::Result<DataBits> {
            self.access().map(|_| DataBits::Eight).map_err(Into::into)
        }

        fn flow_control(&self) -> serialport::Result<FlowControl> {
            self.access().map(|_| FlowControl::Hardware).map_err(Into::into)
        }

        fn parity(&self) -> serialport::Result<Parity> {
            self.access().map(|_| Parity::None).map_err(Into::into)
        }

        fn timeout(&self) -> Duration {
            Duration::from_millis(10)
        }

        fn set_baud_rate(&mut self, _baud_rate: u32) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn stop_bits(&self) -> serialport::Result<StopBits> {
            self.access().map(|_| StopBits::One).map_err(Into::into)
        }

        fn set_data_bits(&mut self, _data_bits: DataBits) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn set_flow_control(&mut self, _flow_control: FlowControl) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn set_parity(&mut self, _parity: Parity) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn set_stop_bits(&mut self, _stop_bits: StopBits) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn set_timeout(&mut self, _timeout: Duration) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            // Read from the Arc to allow tests to set the CTS signal externally
            self.access().map(|_| self.cts.load(Ordering::SeqCst)).map_err(Into::into)
        }

        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            self.access().map(|_| true).map_err(Into::into)
        }

        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            self.access().map(|_| true).map_err(Into::into)
        }

        fn bytes_to_read(&self) -> serialport::Result<u32> {
            self.access().map(|_| 0).map_err(Into::into)
        }

        fn bytes_to_write(&self) -> serialport::Result<u32> {
            self.access().map(|_| 0).map_err(Into::into)
        }

        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            self.access().map(|_| true).map_err(Into::into)
        }

        fn clear(&self, _buffer_to_clear: ClearBuffer) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            self.access().map(|_| Box::new(self.clone()) as _).map_err(Into::into)
        }

        fn set_break(&self) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }

        fn clear_break(&self) -> serialport::Result<()> {
            self.access().map_err(Into::into)
        }
    }

    /// Returns a `Port` that uses a `TestPort`, any error returned while creating it, a handle to
    /// control the CTS signal, and a handle to control the error flag.
    fn initialize_port(
        initial_cts: bool,
    ) -> (Port, serialport::Result<()>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let cts: Arc<AtomicBool> = Arc::new(initial_cts.into());
        let error_flag: Arc<AtomicBool> = Arc::new(false.into());

        let cts_clone: Arc<AtomicBool> = cts.clone();
        let error_flag_clone: Arc<AtomicBool> = error_flag.clone();
        let open_fn = move |_: &str| {
            TestPort::new(cts_clone.clone(), error_flag_clone.clone()).map(|p| Box::new(p) as _)
        };

        let (port, error) = Port::new_with_open_fn(FAKE_PORT.to_string(), Box::new(open_fn));

        (port, error, cts, error_flag)
    }

    #[test]
    fn test_test_port() {
        let cts = Arc::new(true.into());
        let error = Arc::new(false.into());
        let mut port = TestPort::new(cts, error).unwrap();

        assert!(port.write(&[]).is_ok());
        assert!(port.flush().is_ok());
        assert!(port.read(&mut []).is_ok());

        // Accesses succeed despite the `cts` flag being false
        port.cts.store(false, Ordering::SeqCst);

        assert!(port.write(&[]).is_ok());
        assert!(port.flush().is_ok());
        assert!(port.read(&mut []).is_ok());

        // Accesses fail while the `error` flag is true
        port.error.store(true, Ordering::SeqCst);

        assert!(port.write(&[]).is_err());
        assert!(port.flush().is_err());
        assert!(port.read(&mut []).is_err());
    }

    #[test]
    fn test_new() {
        let (_, error) = Port::new(FAKE_PORT.to_string());
        assert!(error.is_err());

        // This opens a port that is inaccessible (its CTS signal is false)
        let (_, error, _, _) = initialize_port(false);
        assert!(error.is_err());

        let (_, error, _, _) = initialize_port(true);
        assert!(error.is_ok());
    }

    #[test]
    fn test_is_accessible() {
        let (mut port, _, _, _) = initialize_port(true);
        assert!(port.device_connected);
        assert!(port.port.is_some());
        assert!(port.is_accessible());

        // If the device disconnects without closing the port, the port is not accessible
        port.device_connected = false;
        assert!(!port.is_accessible());

        // If the port is closed, the port is not accessible (even if `device_connected` is true,
        // which should not occur)
        port.device_connected = true;
        port.port = None;
        assert!(!port.is_accessible());
    }

    #[test]
    fn test_update_port_state() {
        let (mut port, _, cts, error) = initialize_port(true);
        assert!(port.is_accessible());

        // Make the device inaccessible
        cts.store(false, Ordering::SeqCst);

        // The port still thinks it's accessible because no accesses were attempted
        assert!(port.is_accessible());

        // The device is inaccessible after updating the state, but the port is still open
        assert!(port.update_port_state().is_ok());
        assert!(!port.device_connected);
        assert!(port.port.is_some());

        // Make the device accessible again
        cts.store(true, Ordering::SeqCst);

        // Update the port's state again
        assert!(!port.is_accessible());
        assert!(port.update_port_state().is_ok());
        assert!(port.is_accessible());

        // Disconnect the device by causing an error
        error.store(true, Ordering::SeqCst);

        // The port is now fully closed
        assert!(port.update_port_state().is_err());
        assert!(!port.device_connected);
        assert!(port.port.is_none());
    }

    #[test]
    fn test_try_open() {
        let (mut port, _, cts, error) = initialize_port(true);

        assert!(port.try_open().is_ok());

        // Make the device inaccessible
        cts.store(false, Ordering::SeqCst);

        // `try_open` now fails, but the port is still open
        assert!(port.try_open().is_err());
        assert!(!port.is_accessible());
        assert!(port.port.is_some());

        // Make the device accessible again
        cts.store(true, Ordering::SeqCst);

        assert!(port.try_open().is_ok());
        assert!(port.is_accessible());

        // Disconnect the device by causing an error
        error.store(true, Ordering::SeqCst);

        assert!(port.try_open().is_err());
        assert!(port.port.is_none());
        assert!(!port.is_accessible());

        // Subsequent attempts to open the device fail as well
        assert!(port.try_open().is_err());

        // Reconnect the device
        error.store(false, Ordering::SeqCst);

        // Reopen the port
        assert!(port.try_open().is_ok());
        assert!(port.port.is_some());
        assert!(port.is_accessible());
    }

    #[test]
    fn test_io() {
        let (mut port, _, cts, error) = initialize_port(true);

        assert!(port.write(&[]).is_ok());
        assert!(port.read(&mut []).is_ok());

        // Make the device inaccessible
        cts.store(false, Ordering::SeqCst);

        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());

        // Make the device accessible again
        cts.store(true, Ordering::SeqCst);

        assert!(port.write(&[]).is_ok());
        assert!(port.read(&mut []).is_ok());

        // Disconnect the device by causing an error
        error.store(true, Ordering::SeqCst);

        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());
        assert!(port.port.is_none());

        // Reconnect the device
        error.store(false, Ordering::SeqCst);

        // I/O still fails because the port was not reopened
        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());
    }
}
