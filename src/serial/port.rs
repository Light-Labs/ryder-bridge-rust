//! Abstractions for working with serial ports.

use serialport::{ClearBuffer, SerialPort};

use std::io::{self, Write, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A function for opening serial ports given a path.
pub type OpenPortFn = Box<dyn Fn(&Path) -> serialport::Result<Box<dyn SerialPort>> + Send>;

/// A wrapper around a serial port that can be reopened if the serial port is closed.
pub struct Port {
    /// The function to use when opening the serial port.
    open: OpenPortFn,
    /// The path to the serial port.
    path: PathBuf,
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
    pub fn new(path: PathBuf) -> (Self, serialport::Result<()>) {
        Port::new_with_open_fn(path, Box::new(open_serial_port))
    }

    /// Like [`new`][Self::new], but uses a custom function for opening the serial port.
    fn new_with_open_fn(path: PathBuf, open_fn: OpenPortFn) -> (Self, serialport::Result<()>) {
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
        if self.port.is_some() {
            // If the port is already open, check if a device is connected
            match self.update_port_state() {
                Ok(()) => {
                    if self.device_connected {
                        // If one is, do nothing
                        Ok(())
                    } else {
                        // If there is not, return an error
                        Err(get_not_connected_error().into())
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
                Err(get_not_connected_error().into())
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
            let res = f(self.port.as_mut().unwrap().as_mut());
            res
        } else {
            Err(get_not_connected_error())
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
fn open_serial_port(path: &Path) -> serialport::Result<Box<dyn SerialPort>> {
    // The baud rate must be set to 0 on macOS to avoid a bug
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let baud_rate = 0;
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let baud_rate = 115_200;

    let timeout = Duration::from_millis(10);

    serialport::new(path.to_string_lossy(), baud_rate)
        .timeout(timeout)
        .open()
        .and_then(|p| {
            // Clear the serial port buffers to avoid reading garbage data
            p.clear(ClearBuffer::All).map(|_| p)
        })
}

/// Returns whether a device is connected to the serial port, or `Err` if it could not be accessed.
fn is_device_connected<P: SerialPort + ?Sized>(port: &mut P) -> serialport::Result<bool> {
    // Read DSR signal on Windows
    // Also read it during tests to allow all port logic to be tested properly
    #[cfg(any(target_os = "windows", test))]
    return port.read_data_set_ready();
    // No flow control is used on unix-like systems, but the port must still be accessed in some
    // way to detect errors
    #[cfg(not(any(target_os = "windows", test)))]
    return port.bytes_to_read().map(|_| true);
}

/// Returns an error for when a serial port is open, but no device is connected to it.
fn get_not_connected_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "The serial port is open but no device is connected",
    )
}


#[cfg(test)]
mod tests {
    use crate::mock::serial::TestPort;

    use super::*;

    /// Returns a `Port` that uses a `TestPort` internally, a handle to control the `TestPort`, and
    /// any error returned while creating it,
    fn initialize_port(
        initial_dsr: bool,
    ) -> (Port, TestPort, serialport::Result<()>) {
        let test_port = TestPort::new().unwrap();
        test_port.set_device_dsr(initial_dsr);

        let handle = test_port.clone();

        let open_fn = move |_: &Path| {
            Ok(Box::new(test_port.clone()) as _)
        };

        let (port, error) = Port::new_with_open_fn(
            Path::new("./nonexistent").into(),
            Box::new(open_fn),
        );

        (port, handle, error)
    }

    #[test]
    fn test_new() {
        let open_fn = |_: &Path| Err(io::Error::from(io::ErrorKind::NotFound).into());
        let (_, error) = Port::new_with_open_fn(
            Path::new("./nonexistent").into(),
            Box::new(open_fn),
        );
        assert!(error.is_err());

        // This opens a port that is inaccessible (it reads the DSR signal as false)
        let (_, _, error) = initialize_port(false);
        assert!(error.is_err());

        let (_, _, error) = initialize_port(true);
        assert!(error.is_ok());
    }

    #[test]
    fn test_is_accessible() {
        let (mut port, _, _) = initialize_port(true);
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
        let (mut port, handle, _) = initialize_port(true);
        assert!(port.is_accessible());

        // Make the device inaccessible
        handle.set_device_dsr(false);

        // The port still thinks it's accessible because no accesses were attempted
        assert!(port.is_accessible());

        // The device is inaccessible after updating the state, but the port is still open
        assert!(port.update_port_state().is_ok());
        assert!(!port.device_connected);
        assert!(port.port.is_some());

        // Make the device accessible again
        handle.set_device_dsr(true);

        // Update the port's state again
        assert!(!port.is_accessible());
        assert!(port.update_port_state().is_ok());
        assert!(port.is_accessible());

        // Disconnect the device by causing an error
        handle.set_has_error(true);

        // The port is now fully closed
        assert!(port.update_port_state().is_err());
        assert!(!port.device_connected);
        assert!(port.port.is_none());
    }

    #[test]
    fn test_try_open() {
        let (mut port, handle, _) = initialize_port(true);

        assert!(port.try_open().is_ok());

        // Make the device inaccessible
        handle.set_device_dsr(false);

        // `try_open` now fails, but the port is still open
        assert!(port.try_open().is_err());
        assert!(!port.is_accessible());
        assert!(port.port.is_some());

        // Make the device accessible again
        handle.set_device_dsr(true);

        assert!(port.try_open().is_ok());
        assert!(port.is_accessible());

        // Disconnect the device by causing an error
        handle.set_has_error(true);

        assert!(port.try_open().is_err());
        assert!(port.port.is_none());
        assert!(!port.is_accessible());

        // Subsequent attempts to open the device fail as well
        assert!(port.try_open().is_err());

        // Reconnect the device
        handle.set_has_error(false);

        // Reopen the port
        assert!(port.try_open().is_ok());
        assert!(port.port.is_some());
        assert!(port.is_accessible());
    }

    #[test]
    fn test_io() {
        let (mut port, handle, _) = initialize_port(true);

        assert!(port.write(&[]).is_ok());
        assert!(port.read(&mut []).is_ok());

        // Make the device inaccessible
        handle.set_device_dsr(false);

        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());

        // Make the device accessible again
        handle.set_device_dsr(true);

        assert!(port.write(&[]).is_ok());
        assert!(port.read(&mut []).is_ok());

        // Disconnect the device by causing an error
        handle.set_has_error(true);

        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());
        assert!(port.port.is_none());

        // Reconnect the device
        handle.set_has_error(false);

        // I/O still fails because the port was not reopened
        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());
    }
}
