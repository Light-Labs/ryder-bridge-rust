use std::{
    env,
    io::Error as IoError,
    io::ErrorKind,
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

const RESPONSE_WAIT_USER_CONFIRM: u8 = 10;

async fn handle_connection(
    device_owner: DeviceOwner,
    raw_stream: TcpStream,
    addr: SocketAddr,
    ryder_port: String,
) {
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
    let mut port = serialport::new(ryder_port, 115_200)
        .timeout(Duration::from_millis(10))
        .open()
        .expect("Failed to open port");

    let broadcast_incoming = incoming.try_for_each(|msg| {
        let previous_owner = *device_owner.lock().unwrap();
        println!(
            "previous owner {:?}, addr {:?}, = {:?}",
            previous_owner.unwrap(),
            addr,
            previous_owner.unwrap() == addr
        );
        if true || previous_owner.is_none() || previous_owner.unwrap() == addr {
            // take ownership
            if previous_owner.is_none() {
                *device_owner.lock().unwrap() = Some(addr);
            }
            let data = msg.into_data();
            println!(
                "Forwarding a message from {} to ryder device: {:?}",
                addr, data
            );
            if data.len() > 0 {
                port.write(&data).expect("Write failed!");
            }
            let mut response: Vec<u8> = vec![0; 1000];
            let mut wait_for_response: Option<bool> = None;
            loop {
                let bytes_to_read_result = port.bytes_to_read();
                println!("bytes to read: {:?}", bytes_to_read_result);
                match port.read(response.as_mut_slice()) {
                    Ok(t) => {
                        let mut send_to_client = true;
                        if wait_for_response.is_none() {
                            if response[0] == RESPONSE_WAIT_USER_CONFIRM {
                                println!("waiting for user (size: {})", t);
                                wait_for_response = Some(true);
                                send_to_client = false;
                            } else {
                                wait_for_response = Some(false);
                            }
                        } else {
                            match wait_for_response {
                                None | Some(true) => {
                                    println!("user confirmed (size: {})", t);
                                    wait_for_response = None;
                                    send_to_client = true;
                                }
                                Some(false) => {}
                            }
                        }
                        if send_to_client {
                            println!("read {} bytes", t);
                            tx.unbounded_send(Message::binary(&response[..t])).unwrap();
                        }
                    }
                    Err(ref e) if e.kind() == ErrorKind::TimedOut => match wait_for_response {
                        None | Some(false) => {
                            println!("timeout, end of command");
                            break;
                        }
                        Some(true) => {
                            println!("timeout while waiting for user confirmation");
                        }
                    },
                    Err(e) => eprintln!("{:?}", e),
                }
            }
            future::ok(())
        } else {
            //FIXME- "send (to be defined) busy byte instead of ASCII"
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
    let mut args = env::args();

    let ryder_port = args.nth(1).expect("Ryder port is required");
    let addr = args.nth(0).expect("Listening address is required");

    let state = DeviceOwner::new(Mutex::new(None));

    println!("Listening on: {}", addr);
    println!("Ryder port: {}", ryder_port);

    // Create the event loop and TCP listener we'll accept connections on.
    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket.expect("Failed to bind");

    // Let's spawn the handling of each connection in a separate task.
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(
            state.clone(),
            stream,
            addr,
            ryder_port.clone(),
        ));
    }
    Ok(())
}
