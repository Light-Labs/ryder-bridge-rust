//! A bridge for communication between Ryder devices and applications. The device is exposed via a
//! WebSocket API.

mod connection;
mod serial;
mod queue;

use futures_channel::mpsc;
use futures::{FutureExt, select};
use futures_util::{StreamExt, pin_mut};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{watch, Mutex as TokioMutex};

use std::thread;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::queue::ConnectionQueue;
use crate::serial::Server;
use crate::connection::WSConnection;

/// A token that signals that a `tokio` task is still alive as long as it has not been dropped.
#[derive(Clone)]
pub struct TaskAliveToken(mpsc::Sender<()>);

/// Launches the Ryder Bridge for the given serial port and listening address.
pub async fn launch(
    listening_addr: SocketAddr,
    serial_port_path: String,
) {
    println!("Listening on: {}", listening_addr);
    println!("Ryder port: {}", serial_port_path);

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(listening_addr).await;
    let listener = try_socket.expect("Failed to bind");

    let queue = Arc::new(Mutex::new(ConnectionQueue::new()));
    // Set up channel to wait for all tasks to finish
    let (task_alive_token, mut tasks_finished_listener) = mpsc::channel(1);
    let task_alive_token = TaskAliveToken(task_alive_token);

    let (ctrlc_tx, mut ctrlc_rx) = watch::channel(());
    let ctrlc_rx_copy = ctrlc_rx.clone();

    // Create a serial I/O server
    let (serial_server, serial_client, error) = Server::new(serial_port_path, ctrlc_rx.clone());
    let serial_client = Arc::new(TokioMutex::new(serial_client));

    if let Err(e) = error {
        eprintln!("Failed to open serial port: {}", e);
    }

    let server_handle = thread::spawn(|| serial_server.run());

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
                serial_client.clone(),
                ctrlc_rx_copy.clone(),
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

    // Listen for new connections until ctrl-c is received
    let listen = tokio::spawn(async move {
        pin_mut!(listen);
        select! {
            _ = listen => {},
            _ = ctrlc_rx.changed().fuse() => {},
        }
    });

    // Wait for ctrl-c
    if let Err(e) = signal::ctrl_c().await {
        eprintln!("Failed to wait for ctrl-c signal: {}", e);
    }
    ctrlc_tx.send(()).unwrap();

    // Wait for all existing tasks to finish
    listen.await.unwrap();
    // This will return `None` when all `Sender`s (owned by the tasks) have been dropped
    tasks_finished_listener.next().await;

    // Wait for the serial I/O server to exit
    server_handle.join().unwrap();

    println!("Shutting down");
}
