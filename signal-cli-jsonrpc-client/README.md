# signal-cli-jsonrpc-client

A Rust JSON-RPC client for [signal-cli](https://github.com/AsamK/signal-cli) daemon.

## Origin

The RPC interface and transport code in this crate is based on the example client code from the signal-cli repository:

- <https://github.com/AsamK/signal-cli/blob/master/client/src/jsonrpc.rs>

## Usage

```rust
use signal_cli_jsonrpc_client::{connect_tcp, RpcClient};

// Connect to signal-cli daemon via TCP
let client = connect_tcp("127.0.0.1:7583").await?;

// Use the RpcClient trait methods
let version = client.version().await?;
```

## Features

- TCP transport for connecting to signal-cli daemon
- Unix domain socket transport (on Unix systems)
- Full RPC interface matching signal-cli's JSON-RPC API
- Helper types for sending messages and handling received messages

## License

MIT or Apache 2.0 at your option.
