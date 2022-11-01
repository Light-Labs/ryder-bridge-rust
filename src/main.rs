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

use futures_channel::mpsc;
use futures::FutureExt;
use futures_util::{
    stream::TryStreamExt,
    future,
    StreamExt,
    pin_mut,
    SinkExt
};

use tokio::{net::{TcpListener, TcpStream}, task::JoinHandle, sync::oneshot};
use tungstenite::{protocol::Message, Error};
use tokio_serial::{self, SerialPortBuilderExt};

// FIXME: These are placeholders; figure out actual values
const RESPONSE_DEVICE_BUSY: u8 = 50;
const RESPONSE_DEVICE_READY: u8 = 51;
const RESPONSE_DEVICE_DISCONNECTED: u8 = 52;

async fn handle_connection(
    raw_stream: TcpStream,
    addr: SocketAddr,
    ryder_port: String,
    force_disconnect_tx: SyncSender<()>,
    force_disconnect_rx: Receiver<()>,
) {
    println!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    println!("WebSocket connection established: {}", addr);
    let (tx_ws, rx_ws) = mpsc::unbounded();
    let (mut outgoing, incoming) = ws_stream.split();

    // open the serial port
    let port = tokio_serial::new(ryder_port, 0) // 115_200
        .timeout(Duration::from_millis(10))
        .open_native_async();

    let mut port = match port {
        Ok(p) => p,
        Err(e) => {
            // Notify and disconnect the client
            if outgoing.send(Message::Binary(vec![RESPONSE_DEVICE_BUSY])).await.is_ok() {
                if let Err(e) = outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
            }

            eprintln!("Error opening serial port: {}, {:?}", e, e.kind());
            return;
        }
    };

    // For sending data to a dedicated thread that communicates with the serial device
    let (tx_serial, mut rx_serial) = mpsc::unbounded();

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

    let serial_io = std::thread::spawn(move || {
        loop {
            if let Ok(()) = force_disconnect_rx.try_recv() {
                return Ok::<(), tokio_serial::Error>(());
            }

            if let Ok(data) = rx_serial.try_next() {
                if let Some(d) = data {
                    port.write(&d)?;
                } else {
                    return Ok::<(), tokio_serial::Error>(());
                }
            }

            let mut buf = vec![0; 256];
            match port.try_read(&mut buf) {
                Ok(bytes) => tx_ws.unbounded_send(Message::Binary(buf[..bytes].to_vec())).unwrap(),
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => continue,
                    _ => {
                        tx_ws.unbounded_send(
                            Message::Binary(vec![RESPONSE_DEVICE_DISCONNECTED])
                        ).unwrap();
                        // Close the WebSocket TX (happens automatically but this makes it clear)
                        drop(tx_ws);
                        return Err(e.into());
                    }
                }
            }
        }
    });

    let receive_from_others = rx_ws.map(Ok).forward(&mut outgoing);

    pin_mut!(broadcast_incoming, receive_from_others);
    // Wait for the client or the serial IO thread to end the connection
    future::select(broadcast_incoming, receive_from_others).await;

    if let Ok(Err(e)) = serial_io.join() {
        eprintln!("Error in serial port I/O: {}, {:?}", e, e.kind());
    }

    if let Err(e) = outgoing.close().await {
        eprintln!("Failed to close WebSocket: {}", e);
    }

    println!("{} disconnected", &addr);
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

    // Let's spawn the handling of each connection in a separate task.
    let listen = async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let mut connections = connections.lock().unwrap();

            // Don't accept new connections while exiting
            if exiting.load(Ordering::SeqCst) {
                break;
            }

            let (force_disconnect_tx, force_disconnect_rx) = std::sync::mpsc::sync_channel(1);
            let handle = tokio::spawn(handle_connection(
                stream,
                addr,
                ryder_port.clone(),
                force_disconnect_tx.clone(),
                force_disconnect_rx,
            ));

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
