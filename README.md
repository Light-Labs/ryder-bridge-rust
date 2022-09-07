# ryder-bridge-rust
Bridge to ryder device in rust

The bridge translates json http requests to serial commands and back.

## Running the bridge
### Linux
On linux following packages are required: rustup/cargo, build-essential, libudev-dev

```
cargo run <ryder-port from simulator> <server url>
```
e.g. 
```
cargo run /dev/pts/10 localhost:8080
``` 
