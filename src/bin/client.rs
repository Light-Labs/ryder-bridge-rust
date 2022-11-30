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

use std::{env, process, error::Error};

use futures::{FutureExt, pin_mut, SinkExt, StreamExt, TryStreamExt, select};
use tokio::io::{AsyncReadExt};
use tokio::signal;
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use futures::channel::mpsc;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let connect_addr = env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("this program requires at least one argument"));

    let url = Url::parse(&connect_addr)?;

    let (stdin_tx, stdin_rx) = mpsc::unbounded();
    tokio::spawn(read_stdin(stdin_tx));

    let (ws_stream, _) = connect_async(url).await?;
    println!("WebSocket handshake has been successfully completed");

    let (mut write, read) = ws_stream.split();

    let stdin_to_ws = stdin_rx.map(|m| {
        if let Ok(s) = String::from_utf8(m) {
            Ok(Message::binary(parse_input(&s)))
        } else {
            Ok(Message::binary([]))
        }
    }).forward(&mut write);
    let ws_to_stdout = {
        read.try_for_each(|message| async move {
            if let Message::Binary(data) = message {
                println!("response: {:?}", data);
            } else {
                println!("response: {:?}", message);
            }
            Ok(())
        }).fuse()
    };

    pin_mut!(stdin_to_ws, ws_to_stdout);
    select!(
        _ = stdin_to_ws => {},
        _ = ws_to_stdout => {},
        // Watch for ctrl-c
        res = signal::ctrl_c().fuse() => if let Err(e) = res {
            eprintln!("Failed to wait for ctrl-c signal: {}", e);
        },
    );

    if let Err(e) = write.close().await {
        eprintln!("Failed to close WebSocket connection: {}", e);
    }
    println!("disconnected");
    process::exit(0);
}

// Our helper method which will read data from stdin and send it along the
// sender provided.
async fn read_stdin(tx: futures::channel::mpsc::UnboundedSender<Vec<u8>>) {
    let mut stdin = tokio::io::stdin();
    let mut input = Vec::new();
    let mut buf = vec![0; 1024];
    loop {
        let bytes = match stdin.read(&mut buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };

        // Add the most recent input to any previously stored input
        input.extend(&buf[..bytes]);
        let mut start = 0;

        // Get all newline-delimited inputs
        while let Some((i, _)) = input[start..].iter().enumerate().find(|(_, &c)| c == b'\n') {
            tx.unbounded_send(input[start..i + 1].to_vec()).unwrap();

            // If the newline was not the last character, continue searching the remaining data
            start = i + 1;
            if i == input.len() - 1 {
                break;
            }
        }

        // Carry over incomplete data to the next iteration
        if start < input.len() {
            let remaining = input[start..].to_vec();
            input.clear();
            input.extend(remaining);
        } else {
            input.clear();
        }
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
