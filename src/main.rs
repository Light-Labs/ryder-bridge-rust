use std::{
    env,
    io::Error as IoError,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_channel::mpsc::unbounded;
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt};

use tokio::net::{TcpListener, TcpStream};
use tungstenite::protocol::Message;

use serialport;

type DeviceOwner = Arc<Mutex<Option<SocketAddr>>>;

async fn handle_connection(device_owner: DeviceOwner, raw_stream: TcpStream, addr: SocketAddr) {
    let previous_owner = *device_owner.lock().unwrap();
    if previous_owner.is_none() {
        *device_owner.lock().unwrap() = Some(addr);
    }

    println!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    println!("WebSocket connection established: {}", addr);
    let (tx, rx) = unbounded();
    let (outgoing, incoming) = ws_stream.split();

    // open the serial port
    let mut port = serialport::new("/dev/pts/7", 115_200)
        .timeout(Duration::from_millis(10))
        .open()
        .expect("Failed to open port");

    let broadcast_incoming = incoming.try_for_each(|msg| {
        let previous_owner = *device_owner.lock().unwrap();
        if previous_owner.is_none() || previous_owner.unwrap() == addr {
            *device_owner.lock().unwrap() = Some(addr);

            let data = msg.into_data();
            println!("Received a message from {}: {:?}", addr, data);
            port.write(&data).expect("Write failed!");
            let mut response: Vec<u8> = vec![0; 1000];
            match port.read(response.as_mut_slice()) {
                Ok(t) => {
                    println!("read {} bytes", t);
                    tx.unbounded_send(Message::binary(&response[..t])).unwrap();
                }
                Err(e) => eprintln!("{:?}", e),
            }
            future::ok(())
        } else {
            tx.unbounded_send(Message::text("Ryder in use")).unwrap();
            future::ok(())
        }
    });

    let receive_from_others = rx.map(Ok).forward(outgoing);

    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    println!("{} disconnected", &addr);
    *device_owner.lock().unwrap() = None;
}

#[tokio::main]
async fn main() -> Result<(), IoError> {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    let state = DeviceOwner::new(Mutex::new(None));

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket.expect("Failed to bind");
    println!("Listening on: {}", addr);
    // Let's spawn the handling of each connection in a separate task.
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(state.clone(), stream, addr));
    }

    Ok(())
}
