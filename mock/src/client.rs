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
    incoming: WSIncomingStream,
    outgoing: WSOutgoingSink,
    /// Whether the connection has been closed.
    terminated: bool,
}

impl WSClient {
    /// Returns a new `WSClient` that connects to `ws://localhost:port`.
    pub async fn new(port: u16) -> Result<Self, Error> {
        let url = Url::parse(&format!("ws://localhost:{}", port)).unwrap();
        let (stream, _) = select! {
            s = tokio_tungstenite::connect_async(url).fuse() => s?,
            _ = time::sleep(Duration::from_millis(1000)).fuse() => {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
        };

        let (outgoing, incoming) = stream.split();

        Ok(WSClient {
            incoming,
            outgoing,
            terminated: false,
        })
    }

    /// Sends `message` to the bridge.
    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        self.outgoing.send(message).await
    }

    /// Waits for the next response from the bridge and returns `Some(Ok)` if one was received,
    /// `Some(Err)` if there was an error, or `None` if there was no error but no more responses
    /// will be received.
    pub async fn next_response(&mut self) -> Option<Result<Message, Error>> {
        // Only call `next` if `None` has not been returned and the connection is still open
        if !self.terminated {
            let res = self.incoming.next().await;

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
        if !self.terminated {
            self.terminated = true;
            self.outgoing.close().await
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
