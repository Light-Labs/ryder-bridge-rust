# ryder-bridge

A bridge to facilitate connections between Ryder devices and other applications through a WebSocket interface.

## Building and running

Requires Rust 1.61 or higher.

First, clone the repository:

```
git clone https://github.com/Light-Labs/ryder-bridge-rust/
```

Then, the bridge can be built with:

```
cargo build --release
```

and ran with:

```
./target/release/ryder-bridge <device serial port> <listening address>
```

where `<device serial port>` is the path to the serial port of the Ryder device or simulator. Applications can then connect to the bridge through `<listening address>`.

A simple bridge client is provided for testing, and can be ran with:

```
cargo run --bin client -- <WebSocket address>
```

where `<WebSocket address>` is a URL to the bridge, such as `ws://localhost:8888`. 2-digit hexadecimal inputs can be entered to be sent to the bridge, and any responses from the bridge or device are printed.

### Running tests

Unit and integration tests can be ran with:

```
cargo test --all -- --test-threads 1
```

or simply `make test`. The tests will launch the bridge with a default listening port of 8080, but this can be changed by setting the `RYDER_BRIDGE_TEST_PORT` environment variable.

## Bridge responses for clients

The bridge may return several responses in addition to those received from the Ryder device itself. All of these extra responses are text messages, whereas device responses are binary messages.

Multiple clients may connect to the bridge at one time, and they will be placed in a queue and served in the order they connected in.

When a client connects, it will receive one of two responses:

- `RESPONSE_WAIT_IN_QUEUE` if another client currently has access to the device and this client must wait in the queue.
- `RESPONSE_BEING_SERVED` otherwise.

If the client is placed in the queue, it will eventually receive one of two responses:

- `RESPONSE_BEING_SERVED` if the client has moved to the front of the queue and is now being served.
- `RESPONSE_BRIDGE_SHUTDOWN` if the bridge itself is shutting down. The client will be disconnected.

Once `RESPONSE_BEING_SERVED` has been returned, all future responses will be one of the following:

- `RESPONSE_BRIDGE_SHUTDOWN` if the bridge itself is shutting down. The client will be disconnected.
- `RESPONSE_DEVICE_NOT_CONNECTED` if the device is not currently connected. This is only returned immediately after `RESPONSE_BEING_SERVED`. The client will be disconnected.
- `RESPONSE_DEVICE_DISCONNECTED` if the device has disconnected. This is returned only in cases where `RESPONSE_DEVICE_NOT_CONNECTED` is not. The client will be disconnected.

All other responses are binary messages from the device.
