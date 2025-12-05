//! Log message schema used by this crate

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
