//! Log message schema and types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, de};

/// Log severity level, following syslog conventions.
///
/// Lower values indicate higher severity. The ordering allows comparisons
/// like `level <= Level::ERROR` to match ERROR, CRITICAL, ALERT, and EMERGENCY.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Level {
    /// System is unusable.
    EMERGENCY = 0,
    /// Action must be taken immediately.
    ALERT = 1,
    /// Critical conditions.
    CRITICAL = 2,
    /// Error conditions.
    ERROR = 3,
    /// Warning conditions.
    WARNING = 4,
    /// Normal but significant condition.
    NOTICE = 5,
    /// Informational messages.
    INFO = 6,
    /// Debug-level messages.
    DEBUG = 7,
    /// Trace-level messages (more verbose than debug).
    TRACE = 8,
}

impl Level {
    // Convert to our own all-caps string that fits in 5 chars
    pub(crate) fn to_str(self) -> &'static str {
        match self {
            Self::EMERGENCY => "EMERG",
            Self::ALERT => "ALERT",
            Self::CRITICAL => "CRIT",
            Self::ERROR => "ERROR",
            Self::WARNING => "WARN",
            Self::NOTICE => "NOTE",
            Self::INFO => "INFO",
            Self::DEBUG => "DEBUG",
            Self::TRACE => "TRACE",
        }
    }

    /// Parse a level from a string (case-insensitive).
    ///
    /// Accepts various common aliases:
    /// - emergency, emerg
    /// - alert
    /// - critical, crit, fatal
    /// - error, err
    /// - warning, warn
    /// - notice, note
    /// - info, information
    /// - debug
    /// - trace
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "EMERGENCY" | "EMERG" => Some(Self::EMERGENCY),
            "ALERT" => Some(Self::ALERT),
            "CRITICAL" | "CRIT" | "FATAL" => Some(Self::CRITICAL),
            "ERROR" | "ERR" => Some(Self::ERROR),
            "WARNING" | "WARN" => Some(Self::WARNING),
            "NOTICE" | "NOTE" => Some(Self::NOTICE),
            "INFO" | "INFORMATION" => Some(Self::INFO),
            "DEBUG" => Some(Self::DEBUG),
            "TRACE" => Some(Self::TRACE),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Level::parse(&s).ok_or_else(|| {
            de::Error::custom(format!(
                "unknown log level '{}', expected one of: emergency, alert, critical, error, warning, notice, info, debug, trace",
                s
            ))
        })
    }
}

/// A structured log message.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LogMessage {
    /// Severity level of the message.
    pub level: Level,
    /// Unix timestamp in seconds (from the log source).
    pub timestamp: Option<i64>,
    /// Nanosecond component of the timestamp.
    pub timestamp_nanos: u32,
    /// When this message was collected by the gateway.
    pub collected_at: DateTime<Utc>,
    /// Hostname where the log originated.
    pub hostname: Option<Box<str>>,
    /// Application name that generated the log.
    pub appname: Option<Box<str>>,
    /// The log message text.
    pub msg: Box<str>,
    /// Module path (e.g., `myapp::server::handler`).
    pub module_path: Option<Box<str>>,
    /// Source file path.
    pub file: Option<Box<str>>,
    /// Line number in the source file.
    pub line: Option<Box<str>>,
}

impl LogMessage {
    /// Create a builder for constructing a log message.
    pub fn builder(level: Level, msg: impl Into<Box<str>>) -> LogMessageBuilder {
        LogMessageBuilder {
            level,
            msg: msg.into(),
            timestamp: None,
            timestamp_nanos: 0,
            hostname: None,
            appname: None,
            module_path: None,
            file: None,
            line: None,
        }
    }

    /// Get the timestamp in seconds, using the source timestamp if available,
    /// otherwise falling back to the collection time.
    pub fn get_timestamp_or_fallback(&self) -> i64 {
        self.timestamp
            .unwrap_or_else(|| self.collected_at.timestamp())
    }
}

/// Builder for constructing [`LogMessage`] instances.
#[derive(Clone, Debug)]
pub struct LogMessageBuilder {
    level: Level,
    msg: Box<str>,
    timestamp: Option<i64>,
    timestamp_nanos: u32,
    hostname: Option<Box<str>>,
    appname: Option<Box<str>>,
    module_path: Option<Box<str>>,
    file: Option<Box<str>>,
    line: Option<Box<str>>,
}

impl LogMessageBuilder {
    /// Set the Unix timestamp in seconds.
    pub fn timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Set the nanosecond component of the timestamp.
    pub fn timestamp_nanos(mut self, nanos: u32) -> Self {
        self.timestamp_nanos = nanos;
        self
    }

    /// Set the hostname.
    pub fn hostname(mut self, hostname: impl Into<Box<str>>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    /// Set the application name.
    pub fn appname(mut self, appname: impl Into<Box<str>>) -> Self {
        self.appname = Some(appname.into());
        self
    }

    /// Set the module path.
    pub fn module_path(mut self, module_path: impl Into<Box<str>>) -> Self {
        self.module_path = Some(module_path.into());
        self
    }

    /// Set the source file path.
    pub fn file(mut self, file: impl Into<Box<str>>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Set the line number.
    pub fn line(mut self, line: impl Into<Box<str>>) -> Self {
        self.line = Some(line.into());
        self
    }

    /// Build the log message.
    pub fn build(self) -> LogMessage {
        LogMessage {
            level: self.level,
            timestamp: self.timestamp,
            timestamp_nanos: self.timestamp_nanos,
            collected_at: Utc::now(),
            hostname: self.hostname,
            appname: self.appname,
            msg: self.msg,
            module_path: self.module_path,
            file: self.file,
            line: self.line,
        }
    }
}

impl From<LogMessageBuilder> for LogMessage {
    fn from(builder: LogMessageBuilder) -> Self {
        builder.build()
    }
}

/// Identifies the source of log messages (app name + host).
///
/// Used to separate log buffers and rate limiters per source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Origin {
    /// Application name.
    pub app: Box<str>,
    /// Hostname.
    pub host: Box<str>,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.app, self.host)
    }
}

impl From<&LogMessage> for Origin {
    fn from(msg: &LogMessage) -> Self {
        Self {
            app: msg.appname.clone().unwrap_or_default(),
            host: msg.hostname.clone().unwrap_or_default(),
        }
    }
}

impl Origin {
    /// Check if this origin matches a filter string.
    ///
    /// If the filter contains '@', it is split on the first '@':
    /// - The part before '@' must be a substring of `app`
    /// - The part after '@' must be a substring of `host`
    ///
    /// If the filter does not contain '@', it matches if either `app` or `host`
    /// contains the filter string.
    pub fn matches_filter(&self, filter: &str) -> bool {
        if let Some((app_filter, host_filter)) = filter.split_once('@') {
            self.app.contains(app_filter) && self.host.contains(host_filter)
        } else {
            self.app.contains(filter) || self.host.contains(filter)
        }
    }
}

/// Filter criteria for matching log messages.
///
/// All non-empty fields must match for the filter to pass.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LogFilter {
    /// If non-empty, the message must contain this substring.
    #[serde(default)]
    pub msg_contains: String,
    /// If non-empty, the module path must equal this value exactly.
    #[serde(default)]
    pub module_equals: String,
    /// If non-empty, the file path must equal this value exactly.
    #[serde(default)]
    pub file_equals: String,
    /// If non-empty, the line number must equal this value exactly.
    #[serde(default)]
    pub line_equals: String,
}

impl LogFilter {
    /// Check if a log message matches this filter.
    ///
    /// Returns true if all non-empty filter fields match the log message.
    pub fn matches(&self, log_msg: &LogMessage) -> bool {
        if !self.msg_contains.is_empty() && !log_msg.msg.contains(&self.msg_contains) {
            return false;
        }

        if !self.module_equals.is_empty() {
            match log_msg.module_path.as_deref() {
                Some(module) if module == self.module_equals.as_str() => {}
                _ => return false,
            }
        }

        if !self.file_equals.is_empty() {
            match log_msg.file.as_deref() {
                Some(file) if file == self.file_equals.as_str() => {}
                _ => return false,
            }
        }

        if !self.line_equals.is_empty() {
            match log_msg.line.as_deref() {
                Some(line) if line == self.line_equals.as_str() => {}
                _ => return false,
            }
        }

        true
    }

    /// Returns true if this filter uses the module field
    pub fn uses_module(&self) -> bool {
        !self.module_equals.is_empty()
    }

    /// Returns true if this filter uses the file field
    pub fn uses_file(&self) -> bool {
        !self.file_equals.is_empty()
    }

    /// Returns true if this filter uses the line field
    pub fn uses_line(&self) -> bool {
        !self.line_equals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_origin_matches_filter() {
        let origin = Origin {
            app: "muad-dib".into(),
            host: "tokyo-server".into(),
        };

        // Without @: matches if app OR host contains the string
        assert!(origin.matches_filter("muad"));
        assert!(origin.matches_filter("dib"));
        assert!(origin.matches_filter("tokyo"));
        assert!(origin.matches_filter("server"));
        assert!(!origin.matches_filter("paris"));

        // With @: app must contain first part AND host must contain second part
        assert!(origin.matches_filter("muad@tokyo"));
        assert!(origin.matches_filter("dib@server"));
        assert!(origin.matches_filter("muad-dib@tokyo-server"));
        assert!(!origin.matches_filter("muad@paris"));
        assert!(!origin.matches_filter("other@tokyo"));

        // Empty parts with @
        assert!(origin.matches_filter("@tokyo")); // empty app filter matches any app
        assert!(origin.matches_filter("muad@")); // empty host filter matches any host
        assert!(origin.matches_filter("@")); // both empty, matches everything

        // Edge case: filter matches the @ in the format but origin has no @
        let origin2 = Origin {
            app: "app".into(),
            host: "host".into(),
        };
        assert!(origin2.matches_filter("app@host"));
        assert!(!origin2.matches_filter("app@other"));
    }
}
