mod serial;
mod queue;

use std::{
    env,
    net::SocketAddr,
    sync::{
        Arc,
        Mutex,
    },
    thread,
    error,
};

use futures_channel::{mpsc::{self, Sender}, oneshot};
use futures::{FutureExt, select};
use futures_util::{
    stream::TryStreamExt,
    StreamExt,
    pin_mut,
    SinkExt
};

use tokio::{net::{TcpListener, TcpStream}, signal, sync::{watch, Mutex as TokioMutex}};
use tungstenite::{protocol::Message};

use crate::queue::ConnectionQueue;
use crate::serial::{Client, DeviceState, Server};

// FIXME: These are placeholders; figure out actual values
const RESPONSE_DEVICE_BUSY: &'static str = "RESPONSE_DEVICE_BUSY";
const RESPONSE_DEVICE_READY: &'static str = "RESPONSE_DEVICE_READY";
const RESPONSE_DEVICE_DISCONNECTED: &'static str = "RESPONSE_DEVICE_DISCONNECTED";
const RESPONSE_DEVICE_ERROR: &'static str = "RESPONSE_DEVICE_ERROR";
const RESPONSE_BRIDGE_SHUTDOWN: &'static str = "RESPONSE_BRIDGE_SHUTDOWN";

async fn handle_connection(
    raw_stream: TcpStream,
    addr: SocketAddr,
    serial_client: Arc<TokioMutex<Client>>,
    mut ctrlc_rx: watch::Receiver<()>,
    mut ticket_rx: oneshot::Receiver<()>,
    task_alive_token: Sender<()>,
) {
    println!("Incoming TCP connection from: {}", addr);

    // Open the WebSocket connection
    let ws_stream = match tokio_tungstenite::accept_async(raw_stream).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error during WebSocket handshake: {}", e);
            return;
        }
    };
    println!("WebSocket connection established: {}", addr);

    // Split the connection into two streams
    let (mut outgoing, mut incoming) = ws_stream.split();

    // Check if this connection must wait to be served
    if let Ok(None) = ticket_rx.try_recv() {
        // Notify the client that it must wait for the device to become available
        if outgoing.send(Message::text(RESPONSE_DEVICE_BUSY)).await.is_err() {
            // If the message cannot be sent, just close the connection and return
            if let Err(e) = outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }
            return;
        }

        // Wait in the connection queue until this connection is ready to be served or the client
        // disconnects
        let watch_client_dc = (&mut incoming).try_for_each(|msg| async move {
            if let Message::Close(_) = msg {
                Err(tungstenite::Error::ConnectionClosed)
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
                return;
            },
            // This connection is being served now
            _ = ticket_rx => {
                // Notify the client that the device is ready
                if let Err(_) = outgoing.send(Message::text(RESPONSE_DEVICE_READY)).await {
                    if let Err(e) = outgoing.close().await {
                        eprintln!("Failed to close WebSocket: {}", e);
                    }
                    return;
                }
            }
            // The bridge is shutting down
            _ = ctrlc_rx.changed().fuse() => {
                let _ = outgoing.send(Message::text(RESPONSE_BRIDGE_SHUTDOWN)).await;
                if let Err(e) = outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
                return;
            }
        }
    }

    // Take control of the serial client connection
    let mut serial_client = serial_client
        .try_lock()
        .expect("Serial client connection already in use");

    // Destructure the client to avoid borrow checker issues
    let Client {
        ref mut tx,
        ref mut rx,
        ref mut device_state,
    } = *serial_client;
    let serial_tx = tx;
    let serial_rx = rx;

    // If the serial device is not connected, notify the client and return
    if *device_state.borrow() == DeviceState::NotConnected {
        let _ = outgoing.send(Message::text(RESPONSE_DEVICE_ERROR)).await;
        if let Err(e) = outgoing.close().await {
            eprintln!("Failed to close WebSocket: {}", e);
        }
        return;
    }

    // Set up message receiver for the WebSocket
    let ws_receiver = incoming.try_for_each(|msg| {
        async {
            // If the client disconnected, stop listening
            if let Message::Close(_) = msg {
                return Err(tungstenite::Error::ConnectionClosed);
            }

            let data = msg.into_data();
            println!("Received a message from {}: {:?}", addr, data);
            if data.len() > 0 {
                serial_tx.unbounded_send(data).unwrap();
            }

            Ok(())
        }
    }).fuse();

    // Send responses to the WebSocket
    let ws_sender = serial_rx
        .map(|d| {
            Ok(Message::binary(d))
        })
        .forward(&mut outgoing);

    // Wait for a ctrl-c signal or for the client or serial IO server to end the connection
    pin_mut!(ws_receiver, ws_sender);
    loop {
        select! {
            _ = ws_receiver => break,
            _ = ws_sender => break,
            _ = ctrlc_rx.changed().fuse() => {
                let _ = outgoing.send(Message::text(RESPONSE_BRIDGE_SHUTDOWN)).await;
                break;
            },
            _ = device_state.changed().fuse() => {
                if *device_state.borrow() == DeviceState::NotConnected {
                    let _ = outgoing.send(Message::text(RESPONSE_DEVICE_DISCONNECTED)).await;
                    break;
                }
            }
        };
    }

    // Close the WebSocket connection
    if let Err(e) = outgoing.close().await {
        eprintln!("Failed to close WebSocket: {}", e);
    }

    // Signal that this task is completed
    drop(task_alive_token);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn error::Error>> {
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

    // Create a serial I/O server
    let (serial_server, serial_client, error) = Server::new(ryder_port, ctrlc_rx.clone());
    let serial_client = Arc::new(TokioMutex::new(serial_client));

    if let Some(e) = error {
        eprintln!("Failed to open serial port: {}", e);
    }

    let server_handle = thread::spawn(|| serial_server.run());

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
                serial_client.clone(),
                ctrlc_rx_copy.clone(),
                ticket_rx,
                task_alive_token.clone(),
            ).map(move |_| {
                println!("{} disconnected", addr);

                // Remove connections from the queue when they are finished and serve the next in
                // line
                let mut queue = queue_clone.lock().unwrap();
                queue.remove_and_serve_next(id);
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

    // Wait for the serial I/O server to exit
    server_handle.join().unwrap();

    println!("Shutting down");

    Ok(())
}
