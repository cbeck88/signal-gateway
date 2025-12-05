//! Log message schema used by this crate

use serde::Deserialize;

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Level {
    EMERGENCY = 0,
    ALERT = 1,
    CRITICAL = 2,
    ERROR = 3,
    WARNING = 4,
    NOTICE = 5,
    INFO = 6,
    DEBUG = 7,
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
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LogMessage {
    pub level: Level,
    pub timestamp: Option<i64>,
    pub timestamp_nanos: u32,
    pub hostname: Option<Box<str>>,
    pub appname: Option<Box<str>>,
    pub msg: Box<str>,
    pub module_path: Option<Box<str>>,
    pub file: Option<Box<str>>,
    pub line: Option<Box<str>>,
}

impl LogMessage {
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
}

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
    pub fn timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    pub fn timestamp_nanos(mut self, nanos: u32) -> Self {
        self.timestamp_nanos = nanos;
        self
    }

    pub fn hostname(mut self, hostname: impl Into<Box<str>>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn appname(mut self, appname: impl Into<Box<str>>) -> Self {
        self.appname = Some(appname.into());
        self
    }

    pub fn module_path(mut self, module_path: impl Into<Box<str>>) -> Self {
        self.module_path = Some(module_path.into());
        self
    }

    pub fn file(mut self, file: impl Into<Box<str>>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn line(mut self, line: impl Into<Box<str>>) -> Self {
        self.line = Some(line.into());
        self
    }

    pub fn build(self) -> LogMessage {
        LogMessage {
            level: self.level,
            timestamp: self.timestamp,
            timestamp_nanos: self.timestamp_nanos,
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
/// Used to separate log buffers and rate limiters per source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Origin {
    pub app: Box<str>,
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

/// Filter criteria for matching log messages
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LogFilter {
    #[serde(default)]
    pub msg_contains: String,
    #[serde(default)]
    pub module_equals: String,
    #[serde(default)]
    pub file_equals: String,
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
