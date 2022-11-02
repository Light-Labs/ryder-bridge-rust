//! A simple example of hooking up stdin/stdout to a WebSocket stream.
//!
//! This example will connect to a server specified in the argument list and
//! then forward all data read on stdin to the server, printing out all data
//! received on stdout.
//!
//! Note that this is not currently optimized for performance, especially around
//! buffer management. Rather it's intended to show an example of working with a
//! client.
//!
//! You can use this example together with the `server` example.

use std::{env, process};

use futures::{FutureExt, SinkExt, select};
use futures_util::{pin_mut, StreamExt};
use tokio::io::{AsyncReadExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use futures_channel::{mpsc, oneshot};
use url::Url;

#[tokio::main]
async fn main() {
    let connect_addr = env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("this program requires at least one argument"));

    let url = Url::parse(&connect_addr).unwrap();

    let (stdin_tx, stdin_rx) = mpsc::unbounded();
    tokio::spawn(read_stdin(stdin_tx));

    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");
    println!("WebSocket handshake has been successfully completed");

    let (ctrlc_tx, mut ctrlc_rx) = oneshot::channel();
    let mut ctrlc_tx = Some(ctrlc_tx);
    ctrlc::set_handler(move || {
        ctrlc_tx.take().map(|c| c.send(()));
    }).expect("Failed to set ctrl-c handler");

    let (mut write, read) = ws_stream.split();

    let stdin_to_ws = stdin_rx.map(|m| {
        if let Ok(s) = String::from_utf8(m.into_data()) {
            Ok(Message::binary(parse_input(&s)))
        } else {
            Ok(Message::binary([]))
        }
    }).forward(&mut write);
    let ws_to_stdout = {
        read.for_each(|message| async {
            let data = message.unwrap().into_data();
            println!("response: {:?}", data);
        }).fuse()
    };

    pin_mut!(stdin_to_ws, ws_to_stdout);
    select!(
        _ = stdin_to_ws => {},
        _ = ws_to_stdout => {},
        _ = ctrlc_rx => {},

    );

    write.close().await.expect("Failed to close websocket");
    println!("disconnected");
    process::exit(0);
}

// Our helper method which will read data from stdin and send it along the
// sender provided.
async fn read_stdin(tx: futures_channel::mpsc::UnboundedSender<Message>) {
    let mut stdin = tokio::io::stdin();
    loop {
        let mut buf = vec![0; 1024];
        let n = match stdin.read(&mut buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.truncate(n - 1);
        tx.unbounded_send(Message::binary(buf)).unwrap();
    }
}

// Parses a user input string into a byte array, assuming it is a sequence of two digit hexadecimal
// numbers. Spaces and invalid parts of the input are discarded.
fn parse_input(s: &str) -> Vec<u8> {
    s
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|c| u8::from_str_radix(&c.iter().cloned().collect::<String>(), 16))
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use crate::parse_input;

    #[test]
    fn test_parse_input() {
        assert_eq!(Vec::<u8>::new(), parse_input(""));
        assert_eq!(vec![1], parse_input("1"));
        assert_eq!(vec![1], parse_input("01"));
        assert_eq!(vec![1, 2, 3], parse_input("010203"));
        assert_eq!(vec![1, 2, 3], parse_input("01 02 03"));
        assert_eq!(vec![1, 3], parse_input("01 xx 03"));
    }
}
