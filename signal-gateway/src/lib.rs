//! Signal Gateway library for bridging alertmanager and logging with Signal messenger.
//!
//! This crate provides the core functionality for receiving alerts and log messages
//! and forwarding them to Signal messenger via signal-cli.

#![deny(missing_docs)]

pub mod alertmanager;
pub mod assistant;
pub mod gateway;
pub mod message_handler;

pub(crate) mod circular_buffer;
pub(crate) mod concurrent_map;
pub(crate) mod lazy_map_cleaner;
pub(crate) mod limiter_sequence;
pub(crate) mod log_format;
pub(crate) mod log_message;
pub(crate) mod prometheus;
pub(crate) mod rate_limiter;
pub(crate) mod signal_jsonrpc;
pub(crate) mod transports;

pub use gateway::{CommandRouter, CommandRouterBuilder, Gateway, GatewayConfig, Handling};
pub use limiter_sequence::Limit;
pub use log_message::{Level, LogFilter, LogMessage, LogMessageBuilder};
pub use message_handler::{
    AdminMessage, AdminMessageResponse, Context, MessageHandler, MessageHandlerResult,
};
