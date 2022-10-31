use std::{
    env,
    io::{self, Error as IoError, Write},
    net::SocketAddr,
    time::Duration,
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

use tokio::net::{TcpListener, TcpStream};
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
            // tx_ws.unbounded_send(Message::Binary(vec![RESPONSE_DEVICE_BUSY])).unwrap();
            println!("Error opening serial port: {}", e);
            return;
        }
    };

    // For sending data to a dedicated thread that communicates with the serial device
    let (tx_serial, mut rx_serial) = mpsc::unbounded();
    // For detecting client disconnects
    let (disconnect_tx, disconnect_rx) = std::sync::mpsc::sync_channel(1);

    let broadcast_incoming = incoming.try_for_each(|msg| {
        async {
            // If the client disconnected, stop listening and send a signal to close the serial
            // port as well
            if let Message::Close(_) = msg {
                disconnect_tx.clone().send(()).unwrap();
                return Err(Error::ConnectionClosed);
            }

            let data = msg.into_data();
            println!("Received a message from {}: {:?}", addr, data);
            if data.len() > 0 {
                tx_serial.unbounded_send(data).unwrap();
                // if let Err(e) = port.write(&data) {
                //     if let ErrorKind::BrokenPipe = e.kind() {
                //         // Let the client know that the connection was closed
                //         tx.unbounded_send(Message::Binary(vec![RESPONSE_DEVICE_DISCONNECTED])).unwrap();
                //         return future::err(tungstenite::Error::Io(e));
                //     } else {
                //         eprintln!("error: {}", e);
                //     }
                // }
            }
            Ok(())
        }
    }).fuse();

    let serial_io = std::thread::spawn(move || {
        loop {
            if let Ok(()) = disconnect_rx.try_recv() {
                break Ok::<(), tokio_serial::Error>(());
            }

            if let Ok(data) = rx_serial.try_next() {
                if let Some(d) = data {
                    port.write(&d)?;
                } else {
                    break Ok::<(), tokio_serial::Error>(());
                }
            }

            let mut buf = vec![0; 256];
            match port.try_read(&mut buf) {
                Ok(bytes) => tx_ws.unbounded_send(Message::Binary(buf[..bytes].to_vec())).unwrap(),
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => continue,
                    _ => break Err(e.into()),
                }
            }
        }
    });

    let receive_from_others = rx_ws.map(Ok).forward(&mut outgoing);

    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    if let Ok(Err(e)) = serial_io.join() {
        eprintln!("Error in serial port I/O: {}", e);
    }

    outgoing.close().await.unwrap();

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

    // Let's spawn the handling of each connection in a separate task.
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(
            stream,
            addr,
            ryder_port.clone(),
        ));
    }
    Ok(())
}
