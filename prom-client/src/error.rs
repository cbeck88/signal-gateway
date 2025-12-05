//! Error types for prom-client

use displaydoc::Display;
use url::ParseError;

/// Errors that can occur when making Prometheus API requests
#[derive(Debug, Display)]
pub enum Error {
    /// URL: {0}
    Url(ParseError),
    /// Reqwest: {0}
    Reqwest(reqwest::Error),
    /// API: {0}: {1}
    API(String, String),
    /// Unexpected Result Type: {0}
    UnexpectedResultType(String),
    /// Missing data on success response
    MissingData,
}

impl From<reqwest::Error> for Error {
    fn from(src: reqwest::Error) -> Self {
        Self::Reqwest(src)
    }
}

impl From<ParseError> for Error {
    fn from(src: ParseError) -> Self {
        Self::Url(src)
    }
}

impl std::error::Error for Error {}
