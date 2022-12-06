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
