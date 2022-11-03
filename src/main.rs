mod queue;

use std::{
    env,
    io::{self, Error as IoError, Write},
    net::SocketAddr,
    time::Duration,
    sync::{
        Arc,
        Mutex,
        mpsc::{SyncSender, Receiver}, atomic::{AtomicBool, Ordering},
    }, thread,
};

use futures_channel::{mpsc, oneshot};
use futures::{FutureExt, select};
use futures_util::{
    stream::TryStreamExt,
    future,
    StreamExt,
    pin_mut,
    SinkExt
};

use tokio::{net::{TcpListener, TcpStream}, task::JoinHandle};
use tungstenite::{protocol::Message, Error};
use tokio_serial::{self, SerialPortBuilderExt, ErrorKind};

use crate::queue::ConnectionQueue;

// FIXME: These are placeholders; figure out actual values
const RESPONSE_DEVICE_BUSY: u8 = 50;
const RESPONSE_DEVICE_READY: u8 = 51;
const RESPONSE_DEVICE_DISCONNECTED: u8 = 52;
const RESPONSE_DEVICE_NOT_FOUND: u8 = 53;
const RESPONSE_DEVICE_UNKNOWN: u8 = 54;

/// Whether to serve the next connection in the queue after `handle_connection` returns.
enum ServeNextInQueue {
    Yes,
    No,
}

/// Returns whether to serve the next connection in the queue.
async fn handle_connection(
    raw_stream: TcpStream,
    addr: SocketAddr,
    ryder_port: String,
    force_disconnect_tx: SyncSender<()>,
    force_disconnect_rx: Receiver<()>,
    mut ticket_rx: oneshot::Receiver<()>,
) -> ServeNextInQueue {
    println!("Incoming TCP connection from: {}", addr);

    // Open the WebSocket connection
    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    println!("WebSocket connection established: {}", addr);

    // Split the connection into two streams
    let (mut outgoing, mut incoming) = ws_stream.split();

    // Check if this connection must wait to be served
    if let Ok(None) = ticket_rx.try_recv() {
        // Notify the client that it must wait for the device to become available
        if outgoing.send(Message::binary([RESPONSE_DEVICE_BUSY])).await.is_err() {
            // If the message cannot be sent, just close the connection and return
            if let Err(e) = outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }
            // This connection is not the one being served, so don't advance the queue
            return ServeNextInQueue::No;
        }

        // Wait in the connection queue until this connection is ready to be served or the client
        // disconnects
        let watch_client_dc = (&mut incoming).try_for_each(|msg| async move {
            if let Message::Close(_) = msg {
                Err(Error::ConnectionClosed)
            } else {
                // Ignore messages while waiting
                // Messages could be buffered instead, but it seems more reasonable to simply reject
                // them given that the client shouldn't be sending anything until it gains access to the
                // Ryder device anyways
                Ok(())
            }
        }).fuse();

        pin_mut!(watch_client_dc);

        select! {
            // The client disconnected before being served; close the connection and return
            _ = watch_client_dc => {
                if let Err(e) = outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
                // This connection is not the one being served, so don't advance the queue
                return ServeNextInQueue::No;
            },
            // This connection is being served now
            _ = ticket_rx => {
                // Notify the client that the device is ready
                if let Err(_) = outgoing.send(Message::binary([RESPONSE_DEVICE_READY])).await {
                    if let Err(e) = outgoing.close().await {
                        eprintln!("Failed to close WebSocket: {}", e);
                    }
                    // This connection is currently being served, so the queue should be advanced
                    return ServeNextInQueue::Yes;
                }
            }
        }
    }

    // Past this point, this connection is currently being served, and so `ServeNextInQueue::Yes`
    // must be returned to advance the queue.

    // Open the serial port
    let port = tokio_serial::new(ryder_port, 0) // 115_200
        .timeout(Duration::from_millis(10))
        .open_native_async();

    let mut port = match port {
        Ok(p) => p,
        Err(e) => {
            let response = match e.kind() {
                ErrorKind::Io(io::ErrorKind::NotFound) => RESPONSE_DEVICE_NOT_FOUND,
                _ => RESPONSE_DEVICE_UNKNOWN,
            };

            let _ = outgoing.send(Message::binary([response])).await;
            if let Err(e) = outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }

            eprintln!("Error opening serial port: {}, {:?}", e, e.kind());
            return ServeNextInQueue::Yes;
        }
    };

    // For sending data to a dedicated thread that communicates with the serial device
    let (tx_serial, mut rx_serial) = mpsc::unbounded();

    // Set up message receiver for the WebSocket
    let broadcast_incoming = incoming.try_for_each(|msg| {
        async {
            // If the client disconnected, stop listening and send a signal to close the serial
            // port as well
            if let Message::Close(_) = msg {
                force_disconnect_tx.clone().send(()).unwrap();
                return Err(Error::ConnectionClosed);
            }

            let data = msg.into_data();
            println!("Received a message from {}: {:?}", addr, data);
            if data.len() > 0 {
                tx_serial.unbounded_send(data).unwrap();
            }
            Ok(())
        }
    }).fuse();

    // Create channels for communicating with the WebSocket
    let (tx_ws, rx_ws) = mpsc::unbounded();

    // Start thread to handle all serial port communication
    let serial_io = std::thread::spawn(move || {
        loop {
            // Watch for exit signal
            if let Ok(()) = force_disconnect_rx.try_recv() {
                return Ok::<(), tokio_serial::Error>(());
            }

            // Write data to port
            if let Ok(data) = rx_serial.try_next() {
                if let Some(d) = data {
                    port.write(&d)?;
                } else {
                    return Ok::<(), tokio_serial::Error>(());
                }
            }

            // Read data from port as it's received
            let mut buf = vec![0; 256];
            match port.try_read(&mut buf) {
                Ok(bytes) => tx_ws.unbounded_send(Message::Binary(buf[..bytes].to_vec())).unwrap(),
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => continue,
                    _ => {
                        tx_ws.unbounded_send(
                            Message::binary([RESPONSE_DEVICE_DISCONNECTED])
                        ).unwrap();
                        // Close the WebSocket TX (happens automatically but this makes it clear)
                        drop(tx_ws);
                        return Err(e.into());
                    }
                }
            }
        }
    });

    // Send responses to the WebSocket
    let receive_from_others = rx_ws.map(Ok).forward(&mut outgoing);

    // Wait for the client or the serial IO thread to end the connection
    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    // Wait for the serial IO thread to exit
    if let Ok(Err(e)) = serial_io.join() {
        eprintln!("Error in serial port I/O: {}, {:?}", e, e.kind());
    }

    // Close the WebSocket connection
    if let Err(e) = outgoing.close().await {
        eprintln!("Failed to close WebSocket: {}", e);
    }

    ServeNextInQueue::Yes
}

#[tokio::main]
async fn main() -> Result<(), IoError> {
    let mut args = env::args();

    let ryder_port = args.nth(1).expect("Ryder port is required");
    let addr = args.nth(0).expect("Listening address is required");

    println!("Listening on: {}", addr);
    println!("Ryder port: {}", ryder_port);

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket.expect("Failed to bind");

    // Set up ctrl-c handling infrastructure
    struct ConnectionTerminator {
        // A handle to the connection's task
        handle: JoinHandle<()>,
        // A sender to notify the task that it should terminate itself
        sender: SyncSender<()>,
        // Whether a termination signal has already been sent
        signal_sent: bool,
    }
    let connections: Vec<ConnectionTerminator> = Vec::new();
    let connections = Arc::new(Mutex::new(connections));
    let (ctrlc_tx, ctrlc_rx) = std::sync::mpsc::sync_channel(1);

    ctrlc::set_handler(move || {
        ctrlc_tx.send(()).unwrap();
    }).unwrap();

    // Watch for ctrl-c inputs in a separate thread
    let exiting = Arc::new(AtomicBool::new(false));
    let exiting_thread = exiting.clone();
    let (exit_tx, exit_rx) = oneshot::channel();
    let connections_thread = connections.clone();
    thread::spawn(move || {
        loop {
            if exiting_thread.load(Ordering::SeqCst) {
                let mut connections = connections_thread.lock().unwrap();
                let mut all_terminated = true;

                for conn in &mut *connections {
                    if !conn.signal_sent {
                        // Ignore errors because the connection and its receiver may have already
                        // been dropped
                        let _ = conn.sender.send(());
                        conn.signal_sent = true;
                    }

                    if !conn.handle.is_finished() {
                        all_terminated = false;
                    }
                }

                // Exit once all remaining connections have been closed
                if all_terminated {
                    exit_tx.send(()).unwrap();
                    break;
                }
            } else {
                if let Ok(()) = ctrlc_rx.recv() {
                    exiting_thread.store(true, Ordering::SeqCst);
                }
            }
        }
    });

    let queue = Arc::new(Mutex::new(ConnectionQueue::new()));

    // Let's spawn the handling of each connection in a separate task.
    let listen = async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let mut connections = connections.lock().unwrap();

            // Don't accept new connections while exiting
            if exiting.load(Ordering::SeqCst) {
                break;
            }

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
            let (force_disconnect_tx, force_disconnect_rx) = std::sync::mpsc::sync_channel(1);
            let connection_handler = handle_connection(
                stream,
                addr,
                ryder_port.clone(),
                force_disconnect_tx.clone(),
                force_disconnect_rx,
                ticket_rx,
            ).map(move |res| {
                println!("{} disconnected", addr);

                // Remove connections from the queue when they are finished
                let mut queue = queue_clone.lock().unwrap();
                queue.remove(id);

                // Serve the next connection if necessary
                if let ServeNextInQueue::Yes = res {
                    queue.serve_next();
                }
            });

            let handle = tokio::spawn(connection_handler);

            // Register the connection so that it can be terminated properly on ctrl-c
            let conn = ConnectionTerminator {
                handle,
                sender: force_disconnect_tx,
                signal_sent: false,
            };
            connections.push(conn);
        }
    };

    tokio::spawn(listen);

    // Wait for ctrl-c
    exit_rx.await.unwrap();

    Ok(())
}
