# signal-cli-jsonrpc-client

[![Crates.io](https://img.shields.io/crates/v/signal-cli-jsonrpc-client?style=flat-square)](https://crates.io/crates/signal-cli-jsonrpc-client)
[![Crates.io](https://img.shields.io/crates/d/signal-cli-jsonrpc-client?style=flat-square)](https://crates.io/crates/signal-cli-jsonrpc-client)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square)](LICENSE-APACHE)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE-MIT)

[API Docs](https://docs.rs/signal-cli-jsonrpc-client/latest/signal_cli_jsonrpc_client/)

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
