#![deny(missing_docs)]

//! Minimal API for getting time series data from prometheus
//!
//! To use it, instantiate one of the request objects,
//! e.g. QueryRequest or QueryRangeRequest. When it's helpful a builder is provided.
//!
//! Then use `PromRequest` trait and call `send` or `send_with_client`.
//! This takes the prometheus url, and optionally a reqwest client to use.
//!
//! On success, the result is `PromData`. One would usually call `into_matrix()?`
//! or `into_vector()?` as expected for the request that is made.
//!
//! In prometheus, metric labels are just a set of key-value pairs. However,
//! if you are expecting certain structure, you may use any KV object that implements
//! `serde::Deserialize` in the `PromData` that results from the call.
//!
//! When `plot` feature is active, the plot module can be used to plot the timeseries data.
//! This is mostly intended to be used as previews in alert messages.

use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

mod builders;
pub use builders::QueryRangeRequestBuilder;

mod error;
pub use error::Error;

mod labels;
pub use labels::{ExtractLabels, Labels};

mod messages;
use messages::PromResponse;
pub use messages::{
    AlertInfo, AlertStatus, AlertsResponse, MetricTimeseries, MetricVal, MetricValue, PromData,
};

#[cfg(feature = "plot")]
pub mod plot;

mod traits;
pub use traits::PromRequest;

/// Query parameters for /api/v1/query prometheus request
#[derive(Clone, Debug, Serialize)]
pub struct QueryRequest {
    /// The PromQL query string
    pub query: String,
    /// Optional evaluation timestamp (defaults to current time)
    pub time: Option<DateTime<Utc>>,
}

impl PromRequest for QueryRequest {
    const PATH: &str = "/api/v1/query";
    type Output<KV: Clone + Debug + DeserializeOwned> = PromData<KV>;
}

/// Query parameters for /api/v1/query_range prometheus request
/// Use builder to populate it
#[derive(Clone, Debug, Serialize)]
pub struct QueryRangeRequest {
    query: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: f64,
}

impl QueryRangeRequest {
    /// Get builder for query range request with given query
    pub fn builder(query: impl Into<String>) -> QueryRangeRequestBuilder {
        QueryRangeRequestBuilder::new(query.into())
    }
}

impl PromRequest for QueryRangeRequest {
    const PATH: &str = "/api/v1/query_range";
    type Output<KV: Clone + Debug + DeserializeOwned> = PromData<KV>;
}

/// Query parameters for /api/v1/series prometheus request
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct SeriesRequest {
    /// Series selector arguments
    pub matches: MatchList,
}

impl PromRequest for SeriesRequest {
    const PATH: &str = "/api/v1/series";
    type Output<KV: Clone + Debug + DeserializeOwned> = Vec<KV>;
}

/// Query parameters for /api/v1/labels prometheus request
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct LabelsRequest {
    /// Series selector arguments to filter which labels are returned
    pub matches: MatchList,
}

impl PromRequest for LabelsRequest {
    const PATH: &str = "/api/v1/labels";
    type Output<KV: Clone + Debug + DeserializeOwned> = Vec<String>;
}

/// Query parameters for /api/v1/alerts prometheus request
#[derive(Clone, Debug, Serialize)]
pub struct AlertsRequest {}

impl PromRequest for AlertsRequest {
    const PATH: &str = "/api/v1/alerts";
    type Output<KV: Clone + Debug + DeserializeOwned> = AlertsResponse<KV>;
}

/// Represents a sequence of match[]=...,match[]=...
/// query parameters required by parts of prometheus api
#[derive(Clone, Debug)]
pub struct MatchList(Vec<String>);

impl Serialize for MatchList {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for v in &self.0 {
            map.serialize_entry("match[]", v)?;
        }
        map.end()
    }
}

impl From<Vec<String>> for MatchList {
    fn from(src: Vec<String>) -> Self {
        Self(src)
    }
}

impl FromIterator<String> for MatchList {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = String>,
    {
        Self(iter.into_iter().collect())
    }
}
