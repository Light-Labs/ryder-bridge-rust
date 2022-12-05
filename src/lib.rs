//! A bridge for communication between Ryder devices and applications. The device is exposed via a
//! WebSocket API.

mod connection;
mod serial;
mod queue;

pub use connection::{
    RESPONSE_WAIT_IN_QUEUE,
    RESPONSE_BEING_SERVED,
    RESPONSE_DEVICE_DISCONNECTED,
    RESPONSE_DEVICE_NOT_CONNECTED,
    RESPONSE_BRIDGE_SHUTDOWN,
};

use futures::channel::mpsc;
use futures::future::FusedFuture;
use futures::{FutureExt, pin_mut, select, StreamExt};
use tokio::net::TcpListener;
use tokio::{signal, task};
use tokio::sync::{watch, Mutex as TokioMutex, oneshot};
use tokio::task::JoinHandle;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::queue::ConnectionQueue;
use crate::serial::{OpenPort, Server};
use crate::connection::WSConnection;

/// A token that signals that a `tokio` task is still alive as long as it has not been dropped.
#[derive(Clone)]
pub struct TaskAliveToken(mpsc::Sender<()>);

/// A handle to the bridge that can be used to terminate it.
pub struct BridgeHandle(oneshot::Sender<()>);

impl BridgeHandle {
    /// Returns a new `BridgeHandle` and a receiver to wait for the termination signal.
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();

        (BridgeHandle(tx), rx)
    }

    /// Terminates the bridge.
    pub fn terminate(self) {
        // Ignore errors, as the bridge may have already shut down and dropped its receiver
        let _ = self.0.send(());
    }
}

/// Launches the Ryder Bridge for the given serial port and listening address. Returns a
/// handle to the bridge's task that should be `await`ed, and a [`BridgeHandle`] that can be used
/// to control the bridge.
pub fn launch(
    listening_addr: SocketAddr,
    serial_port_path: PathBuf,
) -> (JoinHandle<()>, BridgeHandle) {
    launch_with_port_open_fn(listening_addr, serial_port_path, serial::open_serial_port)
}

/// Like [`launch`], but uses a custom function for opening the serial port.
pub fn launch_with_port_open_fn<F: OpenPort + 'static>(
    listening_addr: SocketAddr,
    serial_port_path: PathBuf,
    port_open_fn: F,
) -> (JoinHandle<()>, BridgeHandle) {
    // Create a handle for the bridge
    let (bridge_handle, terminate_rx) = BridgeHandle::new();

    // Launch the bridge
    let task_handle = tokio::spawn(
        launch_internal(listening_addr, serial_port_path, Box::new(port_open_fn), terminate_rx)
    );

    (task_handle, bridge_handle)
}

/// Launches the Ryder Bridge for the given serial port and listening address. `handle_terminate_rx`
/// is watched for a signal to terminate the bridge and all connections.
async fn launch_internal(
    listening_addr: SocketAddr,
    serial_port_path: PathBuf,
    port_open_fn: Box<dyn OpenPort>,
    handle_terminate_rx: oneshot::Receiver<()>,
) {
    println!("Listening on: {}", listening_addr);
    println!("Ryder port: {}", serial_port_path.display());

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(listening_addr).await;
    let listener = try_socket.expect("Failed to bind");

    let queue = Arc::new(Mutex::new(ConnectionQueue::new()));
    // Set up channel to wait for all tasks to finish
    let (task_alive_token, mut tasks_finished_listener) = mpsc::channel(1);
    let task_alive_token = TaskAliveToken(task_alive_token);

    // Set up channel to send termination signal to connection tasks and serial server
    let (terminate_tx, mut terminate_rx) = watch::channel(());
    let terminate_rx_copy = terminate_rx.clone();

    // Create a serial I/O server
    let (serial_server, serial_client, error) =
        Server::with_port_open_fn(serial_port_path, port_open_fn, terminate_rx.clone());
    let serial_client = Arc::new(TokioMutex::new(serial_client));
    let serial_client_clone = serial_client.clone();

    if let Err(e) = error {
        eprintln!("Failed to open serial port: {}", e);
    }

    let server_handle = task::spawn_blocking(|| serial_server.run());

    // Let's spawn the handling of each connection in a separate task.
    let listen = async move {
        while let Ok((stream, addr)) = listener.accept().await {
            // Add the connection to the queue
            let queue_clone = queue.clone();
            let (id, ticket_rx) = {
                let mut queue = queue_clone.lock().unwrap();
                let is_empty = queue.is_empty();
                let (id, rx) = queue.add_connection();

                // If this is the first connection in the queue, immediately serve it
                if is_empty {
                    queue.serve_next();
                }

                (id, rx)
            };
            // Create a connection handler
            let connection = WSConnection::new(
                stream,
                addr,
                serial_client_clone.clone(),
                terminate_rx_copy.clone(),
                ticket_rx,
                task_alive_token.clone(),
            ).await;

            let handle_connection = async move {
                match connection {
                    Ok(c) => c.process().await,
                    Err(e) => eprintln!("Error creating WebSocket connection: {}", e),
                }

                println!("{} disconnected", addr);

                // Remove connections from the queue when they are finished and serve the next in
                // line
                let mut queue = queue_clone.lock().unwrap();
                queue.remove_and_serve_next(id);
            };

            tokio::spawn(handle_connection);
        }
    }.fuse();

    // Listen for new connections until a termination signal is received
    let listen = tokio::spawn(async move {
        pin_mut!(listen);
        select! {
            _ = listen => {},
            _ = terminate_rx.changed().fuse() => {},
        }
    });

    // Wait for ctrl-c or other termination signal
    let mut terminate_rx = handle_terminate_rx.fuse();
    let mut server_handle = server_handle.fuse();
    loop {
        select! {
            res = signal::ctrl_c().fuse() => {
                if let Err(e) = res {
                    eprintln!("Failed to wait for ctrl-c signal: {}", e);
                }
                break;
            }
            res = &mut terminate_rx => {
                // Only terminate if a signal was actually sent and the bridge handle was not simply
                // dropped
                if let Ok(()) = res {
                    break;
                }
            }
            // Watch for the serial IO thread shutting down
            // This should be impossible, but this will handle it more gracefully in case of a bug
            res = server_handle => {
                eprintln!("Serial IO thread shut down unexpectedly: {:?}", res);
                break;
            }
        }
    }

    // Forward termination signal to all tasks and threads
    terminate_tx.send(()).unwrap();

    // Wait for all existing tasks to finish
    listen.await.unwrap();
    // This will return `None` when all `Sender`s (owned by the tasks) have been dropped
    tasks_finished_listener.next().await;

    // Wait for the serial I/O server to exit if it hasn't already
    if !server_handle.is_terminated() {
        server_handle.await.unwrap();
    }

    // Ensure that the serial `Client` is dropped only after the serial `Server` exits to avoid
    // closed channel issues
    let _ = serial_client;

    println!("Shutting down");
}

#[cfg(test)]
mod tests {
    use tokio::time;

    use std::path::Path;
    use std::time::Duration;

    use mock;

    use super::*;

    /// Launches the bridge for testing using a nonexistent serial port.
    fn launch_bridge_test() -> (JoinHandle<()>, BridgeHandle) {
        launch(mock::get_bridge_test_addr(), Path::new("./nonexistent").into())
    }

    #[tokio::test]
    async fn test_bridge_handle_terminate() {
        let (task_handle, handle) = launch_bridge_test();
        handle.terminate();

        // Give the bridge a small amount of time to terminate
        assert!(time::timeout(Duration::from_millis(100), task_handle).await.is_ok());
    }
}
