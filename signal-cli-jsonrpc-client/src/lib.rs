//! JSON-RPC client for signal-cli daemon.
//!
//! This crate provides a Rust client for communicating with [signal-cli](https://github.com/AsamK/signal-cli)
//! running in JSON-RPC daemon mode. It supports both TCP and Unix domain socket connections.
//!
//! The RPC interface and transport code is based on the example client code from the signal-cli repository:
//! <https://github.com/AsamK/signal-cli/blob/master/client/src/jsonrpc.rs>

mod rpc;
pub(crate) mod transports;
mod trust_set;

pub use jsonrpsee::core::client::SubscriptionClientT;
pub use rpc::{
    DataMessage, Envelope, GroupInfo, Identity, JsonLink, MessageTarget, RecvMessage, RpcClient,
    RpcClientError, SignalMessage, TrustLevel, connect_ipc, connect_tcp,
};
pub use trust_set::{SafetyNumber, SignalTrustSet, Uuid};
