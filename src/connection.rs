//! Handling of and communication with WebSocket connections.

use futures::{FutureExt, SinkExt, StreamExt, TryStreamExt, pin_mut, select};
use futures::stream::{SplitSink, SplitStream};
use tokio::net::TcpStream;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tungstenite::{Error, Message};

use std::net::SocketAddr;
use std::sync::Arc;

use crate::TaskAliveToken;
use crate::serial::{Client, DeviceState};
use crate::queue::TicketNotifier;

// FIXME: These are placeholders; figure out actual values
const RESPONSE_DEVICE_BUSY: &str = "RESPONSE_DEVICE_BUSY";
const RESPONSE_DEVICE_READY: &str = "RESPONSE_DEVICE_READY";
const RESPONSE_DEVICE_DISCONNECTED: &str = "RESPONSE_DEVICE_DISCONNECTED";
const RESPONSE_DEVICE_NOT_CONNECTED: &str = "RESPONSE_DEVICE_NOT_CONNECTED";
const RESPONSE_BRIDGE_SHUTDOWN: &str = "RESPONSE_BRIDGE_SHUTDOWN";

/// An incoming WS stream for receiving data.
type WSIncomingStream = SplitStream<WebSocketStream<TcpStream>>;
/// An outgoing WS sink for sending data.
type WSOutgoingSink = SplitSink<WebSocketStream<TcpStream>, Message>;

/// A WebSocket connection handler.
pub struct WSConnection {
    /// The state of the connection.
    state: State,
    /// A token that signals that the parent `tokio` task is alive as long as it has not been
    /// dropped.
    _task_alive_token: TaskAliveToken,
}

impl WSConnection {
    /// Creates a new `WSConnection` to handle an incoming WebSocket connection from `addr`.
    ///
    /// Returns `Err` with an error and the connection queue ticket notifier if a connection could
    /// not be established.
    pub async fn new(
        raw_stream: TcpStream,
        addr: SocketAddr,
        serial_client: Arc<TokioMutex<Client>>,
        ctrlc_rx: watch::Receiver<()>,
        mut ticket_rx: TicketNotifier,
        task_alive_token: TaskAliveToken,
    ) -> Result<Self, (Error, TicketNotifier)> {
        println!("Incoming TCP connection from: {}", addr);

        // Open the WebSocket connection
        let ws_stream = match tokio_tungstenite::accept_async(raw_stream).await {
            Ok(s) => s,
            Err(e) => return Err((e, ticket_rx)),
        };
        println!("WebSocket connection established: {}", addr);

        let (outgoing, incoming) = ws_stream.split();

        // Check if this connection must wait to be served
        let ticket = ticket_rx.try_recv().unwrap();
        let shared = SharedState::new(
            addr,
            incoming,
            outgoing,
            serial_client,
            ctrlc_rx,
            ticket_rx,
        );
        let state = match ticket {
            None => State::Waiting(Waiting::new(shared)),
            Some(_) => State::Active(Active::new(shared)),
        };

        Ok(WSConnection {
            state,
            _task_alive_token: task_alive_token,
        })
    }

    /// Processes the WebSocket connection until disconnection.
    pub async fn process(mut self) -> TicketNotifier {
        loop {
            match self.state {
                State::Waiting(s) => {
                    let active = match s.wait_in_queue().await {
                        Ok(a) => a,
                        Err(t) => return t,
                    };
                    self.state = State::Active(active);
                }
                State::Active(s) => return s.process().await,
            }
        }
    }
}

/// The state of a WebSocket connection.
enum State {
    /// See [`Waiting`].
    Waiting(Waiting),
    /// See [`Active`].
    Active(Active),
}

/// The state of the connection when it is waiting in the queue for access to the device.
struct Waiting {
    shared: SharedState,
}

impl Waiting {
    fn new(shared: SharedState) -> Self {
        Waiting {
            shared,
        }
    }

    /// Waits in the queue until this connection is being served and returns the [`Active`] state.
    ///
    /// Returns `Err` with the connection queue ticket notifier if the connection was closed for any
    /// reason before being served.
    async fn wait_in_queue(mut self) -> Result<Active, TicketNotifier> {
        // Notify the client that it must wait for the device to become available
        if self.shared.ws_outgoing.send(Message::text(RESPONSE_DEVICE_BUSY)).await.is_err() {
            // If the message cannot be sent, just close the connection and return
            if let Err(e) = self.shared.ws_outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }
            return Err(self.shared.ticket_rx);
        }

        // Wait in the connection queue until this connection is ready to be served or the client
        // disconnects
        let watch_client_dc = (&mut self.shared.ws_incoming).try_for_each(|msg| async move {
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
                if let Err(e) = self.shared.ws_outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
                return Err(self.shared.ticket_rx);
            },
            // This connection is being served now
            _ = &mut self.shared.ticket_rx => {
                // Notify the client that the device is ready
                if self.shared.ws_outgoing
                    .send(Message::text(RESPONSE_DEVICE_READY))
                    .await
                    .is_err()
                {
                    if let Err(e) = self.shared.ws_outgoing.close().await {
                        eprintln!("Failed to close WebSocket: {}", e);
                    }
                    return Err(self.shared.ticket_rx);
                }
            }
            // The bridge is shutting down
            _ = self.shared.ctrlc_rx.changed().fuse() => {
                let _ = self.shared.ws_outgoing.send(Message::text(RESPONSE_BRIDGE_SHUTDOWN)).await;
                if let Err(e) = self.shared.ws_outgoing.close().await {
                    eprintln!("Failed to close WebSocket: {}", e);
                }
                return Err(self.shared.ticket_rx);
            }
        }

        // The connection is now being served, so return the next state
        Ok(Active::new(self.shared))
    }
}

/// The state of the connection when it has access to the device and is sending and receiving data.
struct Active {
    shared: SharedState,
}

impl Active {
    fn new(shared: SharedState) -> Self {
        Active {
            shared,
        }
    }

    async fn process(mut self) -> TicketNotifier {
        // Take control of the serial client connection
        let mut serial_client = self.shared.serial_client
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
            let _ = self.shared
                .ws_outgoing
                .send(Message::text(RESPONSE_DEVICE_NOT_CONNECTED)).await;
            if let Err(e) = self.shared.ws_outgoing.close().await {
                eprintln!("Failed to close WebSocket: {}", e);
            }
            return self.shared.ticket_rx;
        }

        // Set up message receiver for the WebSocket
        let ws_receiver = self.shared.ws_incoming.try_for_each(|msg| {
            async {
                // If the client disconnected, stop listening
                if let Message::Close(_) = msg {
                    return Err(tungstenite::Error::ConnectionClosed);
                }

                println!("Received a message from {}: {:?}", self.shared.addr, msg);
                let data = msg.into_data();

                // Send data to the serial IO server to be written
                if !data.is_empty() {
                    serial_tx.unbounded_send(data).unwrap();
                }

                Ok(())
            }
        }).fuse();

        // Send responses to the WebSocket
        let ws_sender = serial_rx
            .map(|d| {
                println!("Received a response from the device: {:?}", d);
                Ok(Message::binary(d))
            })
            .forward(&mut self.shared.ws_outgoing);

        // Wait for a ctrl-c signal or for the client or serial IO server to end the connection
        pin_mut!(ws_receiver, ws_sender);
        loop {
            select! {
                _ = ws_receiver => break,
                _ = ws_sender => break,
                _ = self.shared.ctrlc_rx.changed().fuse() => {
                    let _ = self.shared
                        .ws_outgoing
                        .send(Message::text(RESPONSE_BRIDGE_SHUTDOWN)).await;
                    break;
                },
                _ = device_state.changed().fuse() => {
                    if *device_state.borrow() == DeviceState::NotConnected {
                        let _ = self.shared
                            .ws_outgoing
                            .send(Message::text(RESPONSE_DEVICE_DISCONNECTED)).await;
                        break;
                    }
                }
            };
        }

        // Close the WebSocket connection
        if let Err(e) = self.shared.ws_outgoing.close().await {
            eprintln!("Failed to close WebSocket: {}", e);
        }

        // The ticket receiver must not be dropped until the connection is removed from the queue, so it
        // is returned here to the caller
        self.shared.ticket_rx
    }
}

/// Data shared between all connection states.
struct SharedState {
    /// The address of the incoming connection.
    addr: SocketAddr,
    /// The incoming WS stream for receiving data.
    ws_incoming: WSIncomingStream,
    /// The outgoing WS sink for sending data.
    ws_outgoing: WSOutgoingSink,
    /// A handle to the client for the serial port I/O server.
    serial_client: Arc<TokioMutex<Client>>,
    /// A watcher for ctrl-c signals.
    ctrlc_rx: watch::Receiver<()>,
    /// A receiver for the notification that this connection now has access to the device.
    ticket_rx: TicketNotifier,
}

impl SharedState {
    fn new(
        addr: SocketAddr,
        ws_incoming: WSIncomingStream,
        ws_outgoing: WSOutgoingSink,
        serial_client: Arc<TokioMutex<Client>>,
        ctrlc_rx: watch::Receiver<()>,
        ticket_rx: TicketNotifier,
    ) -> Self {
        SharedState {
            addr,
            ws_incoming,
            ws_outgoing,
            serial_client,
            ctrlc_rx,
            ticket_rx,
        }
    }
}

