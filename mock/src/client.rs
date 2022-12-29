//! A mock bridge client.

use futures::{FutureExt, select, SinkExt};
use futures::stream::{SplitSink, SplitStream, StreamExt};
use tokio::net::TcpStream;
use tokio::time;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;

use std::time::Duration;
use std::io;

/// An incoming WS stream for receiving data.
type WSIncomingStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;
/// An outgoing WS sink for sending data.
type WSOutgoingSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// A simple client that connects to the bridge and allows sending data and receiving responses.
pub struct WSClient {
    // `Some` if the connection is open, or `None` otherwise
    connection: Option<(WSOutgoingSink, WSIncomingStream)>,
}

impl WSClient {
    /// Returns a new `WSClient` that connects to `ws://localhost:port`.
    pub async fn new(port: u16) -> Result<Self, Error> {
        let url = Url::parse(&format!("ws://127.0.0.1:{}", port)).unwrap();
        let (stream, _) = select! {
            s = tokio_tungstenite::connect_async(url).fuse() => s?,
            _ = time::sleep(Duration::from_millis(1000)).fuse() => {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
        };

        let (outgoing, incoming) = stream.split();

        Ok(WSClient {
            connection: Some((outgoing, incoming)),
        })
    }

    /// Sends `message` to the bridge.
    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        self.connection.as_mut().unwrap().0.send(message).await
    }

    /// Waits for the next response from the bridge and returns `Some(Ok)` if one was received,
    /// `Some(Err)` if there was an error, or `None` if there was no error but no more responses
    /// will be received.
    pub async fn next_response(&mut self) -> Option<Result<Message, Error>> {
        // Only call `next` if `None` has not been returned and the connection is still open
        if let Some((_, ref mut incoming)) = self.connection {
            let res = incoming.next().await;

            if res.is_none() {
                let _ = self.close();
            }

            res
        } else {
            None
        }
    }

    /// Closes the client's WebSocket connection.
    pub async fn close(&mut self) -> Result<(), Error> {
        // Only close the connection if it is open
        // `take` is used so the connection is dropped to fully close it
        if let Some((mut outgoing, _)) = self.connection.take() {
            outgoing.close().await
        } else {
            Ok(())
        }
    }
}

impl Drop for WSClient {
    fn drop(&mut self) {
        let _ = futures::executor::block_on(self.close());
    }
}
