mod queue;

use std::{
    env,
    io::{self, Error as IoError, Write},
    net::SocketAddr,
    time::Duration,
    sync::{
        Arc,
        Mutex,
    },
    thread,
};

use futures_channel::{mpsc::{self, Sender}, oneshot};
use futures::{FutureExt, select};
use futures_util::{
    stream::TryStreamExt,
    StreamExt,
    pin_mut,
    SinkExt
};

use serialport::ClearBuffer;
use tokio::{net::{TcpListener, TcpStream}, signal, sync::watch};
use tungstenite::{protocol::Message, Error};

use crate::queue::ConnectionQueue;

// FIXME: These are placeholders; figure out actual values
const RESPONSE_DEVICE_BUSY: u8 = 50;
const RESPONSE_DEVICE_READY: u8 = 51;
const RESPONSE_DEVICE_DISCONNECTED: u8 = 52;
const RESPONSE_DEVICE_ERROR: u8 = 53;

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
    mut ctrlc_rx: watch::Receiver<()>,
    mut ticket_rx: oneshot::Receiver<()>,
    task_alive_token: Sender<()>,
) -> ServeNextInQueue {
    println!("Incoming TCP connection from: {}", addr);

    // Open the WebSocket connection
    let ws_stream = match tokio_tungstenite::accept_async(raw_stream).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error during WebSocket handshake: {}", e);
            // Check if this connection is the one being served and signal to advance the queue if
            // so
            let serve_next = if let Ok(Some(_)) = ticket_rx.try_recv() {
                ServeNextInQueue::Yes
            } else {
                ServeNextInQueue::No
            };
            return serve_next;
        }
    };
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
            // The bridge is shutting down
            _ = ctrlc_rx.changed().fuse() => {
                if let Err(e) = outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
                // This connection is not the one being served, so don't advance the queue
                return ServeNextInQueue::No;
            }
        }
    }

    // Past this point, this connection is currently being served, and so `ServeNextInQueue::Yes`
    // must be returned to advance the queue.

    // Open the serial port (baud rate must be 0 on macOS to avoid a bug)
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let baud_rate = 0;
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let baud_rate = 115_200;
    let port = serialport::new(ryder_port, baud_rate)
        .timeout(Duration::from_millis(10))
        .open();

    let mut port = match port {
        Ok(p) => p,
        Err(e) => {
            let _ = outgoing.send(Message::binary([RESPONSE_DEVICE_ERROR])).await;
            if let Err(e) = outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }

            eprintln!("Error opening serial port: {}, {:?}", e, e.kind());
            return ServeNextInQueue::Yes;
        }
    };

    // Clear the serial port buffers to avoid reading garbage data
    let result = port.clear(ClearBuffer::All);
    if let Err(e) = result {
        eprintln!("Failed to clear serial port buffers: {}", e);

        if let Err(e) = outgoing.close().await {
            eprintln!("Failed to close WebSocket: {}", e);
            return ServeNextInQueue::Yes;
        }
    }

    // For sending data to a dedicated thread that communicates with the serial device
    let (tx_serial, mut rx_serial) = mpsc::unbounded();
    // For closing the serial IO thread
    let (close_serial_tx, close_serial_rx) = std::sync::mpsc::sync_channel(1);

    // Set up message receiver for the WebSocket
    let ws_receiver = incoming.try_for_each(|msg| {
        async {
            // If the client disconnected, stop listening and send a signal to close the serial
            // port as well
            if let Message::Close(_) = msg {
                close_serial_tx.send(()).unwrap();
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
    let serial_io = thread::spawn(move || {
        let mut data_to_write = None;
        loop {
            // Watch for exit signal
            if let Ok(()) = close_serial_rx.try_recv() {
                return Ok::<(), serialport::Error>(());
            }

            // Write data to port (prioritizing data that previously failed to write)
            if data_to_write.is_none() {
                data_to_write = rx_serial.try_next().ok();
            }

            if let Some(ref mut data) = data_to_write {
                if let Some(ref mut d) = data {
                    let res = port.write(&d);

                    match res {
                        Ok(bytes) => {
                            // Check if not all bytes were written
                            if bytes < d.len() {
                                d.truncate(d.len() - bytes);
                            } else {
                                data_to_write = None;
                            }
                        }
                        Err(e) => {
                            match e.kind() {
                                // Keep trying if the write did not fail but simply would have
                                // blocked
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::Interrupted => {},
                                // Exit the thread on real failures
                                // FIXME: Notify the client of device disconnection here
                                _ => return Err(e.into()),
                            }
                        }
                    }
                } else {
                    return Ok::<(), serialport::Error>(());
                }
            }

            // Read data from port as it's received
            let mut buf = vec![0; 256];
            match port.read(&mut buf) {
                Ok(bytes) => tx_ws.unbounded_send(Message::Binary(buf[..bytes].to_vec())).unwrap(),
                Err(e) => match e.kind() {
                    // Ignore temporary read failures
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut => continue,
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
    let ws_sender = rx_ws.map(Ok).forward(&mut outgoing);

    // Wait for a ctrl-c signal or for the client or serial IO thread to end the connection
    pin_mut!(ws_receiver, ws_sender);
    select! {
        _ = ws_receiver => {},
        _ = ws_sender => {},
        // Close the serial IO thread on ctrl-c
        _ = ctrlc_rx.changed().fuse() => close_serial_tx.send(()).unwrap(),
    };

    // Wait for the serial IO thread to exit
    if let Ok(Err(e)) = serial_io.join() {
        eprintln!("Error in serial port I/O: {}, {:?}", e, e.kind());
    }

    // Close the WebSocket connection
    if let Err(e) = outgoing.close().await {
        eprintln!("Failed to close WebSocket: {}", e);
    }

    // Signal that this task is completed
    drop(task_alive_token);

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

    let queue = Arc::new(Mutex::new(ConnectionQueue::new()));
    // Set up channel to wait for all tasks to finish
    let (task_alive_token, mut tasks_finished_listener) = mpsc::channel(1);

    let (ctrlc_tx, mut ctrlc_rx) = watch::channel(());
    let ctrlc_rx_copy = ctrlc_rx.clone();

    // Let's spawn the handling of each connection in a separate task.
    let listen = async move {
        let task_alive_token = task_alive_token;
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
            let connection_handler = handle_connection(
                stream,
                addr,
                ryder_port.clone(),
                ctrlc_rx_copy.clone(),
                ticket_rx,
                task_alive_token.clone(),
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

            tokio::spawn(connection_handler);
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

    println!("Shutting down");

    Ok(())
}
