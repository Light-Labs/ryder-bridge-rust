//! A mock serial port implementation.

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A simple serial port implementation that echoes any data written to it. This type is a handle
/// that can be cloned to control the port from multiple locations.
#[derive(Clone)]
pub struct TestPort {
    /// The port's internal buffer.
    buf: Arc<Mutex<Vec<u8>>>,
    /// Whether the port has an error. Simulates a physical disconnect if `true`.
    has_error: Arc<AtomicBool>,
    /// Whether a device is connected to the port. If `false`, the port will not produce errors but
    /// should not be written to or read from. Simulates setting DTR on the device.
    device_dsr: Arc<AtomicBool>,
}

impl TestPort {
    /// Returns a new `TestPort` that has no errors and has a device connected.
    pub fn new() -> serialport::Result<Self> {
        Ok(Self {
            buf: Arc::new(Mutex::new(Vec::new())),
            has_error: Arc::new(false.into()),
            device_dsr: Arc::new(true.into()),
        })
    }

    pub fn has_error(&self) -> bool {
        self.has_error.load(Ordering::SeqCst)
    }

    pub fn set_has_error(&self, has_error: bool) {
        self.has_error.store(has_error, Ordering::SeqCst);
    }

    pub fn device_dsr(&self) -> bool {
        self.device_dsr.load(Ordering::SeqCst)
    }

    pub fn set_device_dsr(&self, device_dsr: bool) {
        self.device_dsr.store(device_dsr, Ordering::SeqCst);
    }

    // Returns `Err` if the `has_error` flag is true, or `Ok` otherwise (even if `device_dsr` is
    // false).
    pub fn try_access(&self) -> io::Result<()> {
        if self.has_error() {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }

    /// Returns a reference to the port's internal buffer.
    fn buf(&self) -> MutexGuard<Vec<u8>> {
        self.buf.lock().unwrap()
    }
}

impl Write for TestPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.try_access().and_then(|_| self.buf.lock().unwrap().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.try_access()
    }
}

impl Read for TestPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.try_access().map(|_| {
            // Simulate a disconnected device by reading nothing
            if !self.device_dsr() {
                return 0;
            }

            let mut port_buf = self.buf();

            // Read bytes equal to the smaller of the lengths of the target buffer and the internal
            // buffer
            let bytes = buf.len().min(port_buf.len());
            buf[..bytes].copy_from_slice(&port_buf[..bytes]);

            // Clear the read bytes
            if bytes == port_buf.len() {
                port_buf.clear();
            } else {
                *port_buf = port_buf[bytes..].to_vec();
            }

            bytes
        })
    }
}

impl SerialPort for TestPort {
    fn name(&self) -> Option<String> {
        None
    }

    fn baud_rate(&self) -> serialport::Result<u32> {
        self.try_access().map(|_| 115_200).map_err(Into::into)
    }

    fn data_bits(&self) -> serialport::Result<DataBits> {
        self.try_access().map(|_| DataBits::Eight).map_err(Into::into)
    }

    fn flow_control(&self) -> serialport::Result<FlowControl> {
        self.try_access().map(|_| FlowControl::Hardware).map_err(Into::into)
    }

    fn parity(&self) -> serialport::Result<Parity> {
        self.try_access().map(|_| Parity::None).map_err(Into::into)
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(10)
    }

    fn set_baud_rate(&mut self, _baud_rate: u32) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn stop_bits(&self) -> serialport::Result<StopBits> {
        self.try_access().map(|_| StopBits::One).map_err(Into::into)
    }

    fn set_data_bits(&mut self, _data_bits: DataBits) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn set_flow_control(&mut self, _flow_control: FlowControl) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn set_parity(&mut self, _parity: Parity) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn set_stop_bits(&mut self, _stop_bits: StopBits) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn set_timeout(&mut self, _timeout: Duration) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        self.try_access().map(|_| true).map_err(Into::into)
    }

    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        self.try_access().map(|_| true).map_err(Into::into)
    }

    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        self.try_access().map(|_| true).map_err(Into::into)
    }

    fn bytes_to_read(&self) -> serialport::Result<u32> {
        self.try_access().map(|_| 0).map_err(Into::into)
    }

    fn bytes_to_write(&self) -> serialport::Result<u32> {
        self.try_access().map(|_| 0).map_err(Into::into)
    }

    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        self.try_access().map(|_| self.device_dsr()).map_err(Into::into)
    }

    fn clear(&self, _buffer_to_clear: ClearBuffer) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        Ok(Box::new(self.clone()))
    }

    fn set_break(&self) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }

    fn clear_break(&self) -> serialport::Result<()> {
        self.try_access().map_err(Into::into)
    }
}

mod tests {
    use super::*;

    #[test]
    fn test_port_new() {
        let port = TestPort::new().unwrap();

        assert!(!port.has_error());
        assert!(port.device_dsr());
    }

    #[test]
    fn test_port_write() {
        let mut port = TestPort::new().unwrap();

        port.write(&[1, 2, 3]).unwrap();
        assert_eq!(&[1, 2, 3][..], &*port.buf());

        port.write(&[4, 5, 6]).unwrap();
        assert_eq!(&[1, 2, 3, 4, 5, 6][..], &*port.buf());
    }

    #[test]
    fn test_port_read() {
        let mut port = TestPort::new().unwrap();

        port.write(&[1, 2, 3]).unwrap();

        let mut buf = [0; 4];
        assert_eq!(3, port.read(&mut buf).unwrap());
        assert_eq!(&[1, 2, 3, 0], &buf);
        // Read data is cleared
        assert!(port.buf().is_empty());

        port.write(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(4, port.read(&mut buf).unwrap());
        assert_eq!(&[1, 2, 3, 4], &buf);
        // Data that could not fit in the target buffer remains
        assert_eq!(&[5, 6][..], &*port.buf());
    }

    #[test]
    fn test_port_error() {
        let mut port = TestPort::new().unwrap();

        port.set_has_error(true);

        assert!(port.try_access().is_err());
        assert!(port.write(&[]).is_err());
        assert!(port.read(&mut []).is_err());
    }

    #[test]
    fn test_port_device_dsr() {
        let mut port = TestPort::new().unwrap();

        assert!(port.read_data_set_ready().unwrap());

        port.set_device_dsr(false);

        assert!(!port.read_data_set_ready().unwrap());

        // The port can be accessed even when no device is connected, but no data will ever be
        // returned to be read
        assert!(port.try_access().is_ok());
        assert!(port.write(&[1, 2, 3]).is_ok());
        assert_eq!(&[1, 2, 3][..], &*port.buf());

        let mut buf = [0; 3];
        assert_eq!(0, port.read(&mut buf).unwrap());
        assert_eq!([0; 3], buf);
    }

    #[test]
    fn test_port_clone() {
        let port = TestPort::new().unwrap();
        let mut port_clone = port.clone();

        port_clone.write(&[1, 2, 3]).unwrap();
        port_clone.set_device_dsr(false);
        port_clone.set_has_error(true);

        // Changes to a clone of the port affect the original copy
        assert_eq!(&[1, 2, 3][..], &*port.buf());
        assert!(!port.device_dsr());
        assert!(port.has_error());
    }
}
