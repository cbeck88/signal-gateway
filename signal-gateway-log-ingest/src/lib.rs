//! Log ingestion modules for signal-gateway.
//!
//! This crate provides listeners for receiving log messages in various formats:
//! - JSON (logstash-compatible) over UDP and TCP
//! - Syslog (RFC 5424) over UDP and TCP

pub mod json;
pub mod path_prefix;
pub mod syslog;

pub use json::JsonConfig;
pub use path_prefix::StripPathPrefixes;
pub use syslog::SyslogConfig;
