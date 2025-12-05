//! Message types for Prometheus API responses
//!
//! The prometheus http interface (at port 9090) renders graphs in JS and gets the raw data using API requests like this:
//!
//! GET http://localhost:9090/api/v1/query_range?query=tick_time{quantile="0.99"}&step=14&start=1762534433.802&end=1762538033.802
//!
//! Response is:
//!
//! {
//!   status: "success"
//!   data: {
//!     resultType: "matrix",
//!     result: [
//!       {
//!         metric: { __name__: "tick_time", instance: "x.y.z.w", job: "ec2", .. },
//!         values: [
//!            [ 1762534433.802, "1.8974293514080933" ],
//!            [ 1762534447.802, "2.029724353457351" ],
//!            ..
//!         ]
//!       }
//!     ]
//!   }
//! }
//!
//! For more detail see:
//! https://prometheus.io/docs/prometheus/latest/querying/api/

use crate::Error;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use std::{
    collections::HashMap,
    fmt::{self, Debug, Display},
};
use tracing::warn;

/// Wrapper for f64 that deserializes from a string (prometheus returns numeric values as strings)
#[derive(Clone, Copy, Debug)]
pub struct MetricVal(pub f64);

impl Display for MetricVal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl AsRef<f64> for MetricVal {
    fn as_ref(&self) -> &f64 {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MetricVal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <&str>::deserialize(deserializer)?;
        s.parse::<f64>()
            .map(MetricVal)
            .map_err(serde::de::Error::custom)
    }
}

/// A single metric value from an instant query
#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct MetricValue<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    /// The metric labels
    pub metric: KV,
    /// The timestamp and value (if present)
    #[serde(default)]
    pub value: Option<(f64, MetricVal)>,
    // TODO: Include histograms
}

/// A metric timeseries from a range query
#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct MetricTimeseries<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    /// The metric labels
    pub metric: KV,
    /// The timestamp/value pairs
    #[serde(default)]
    pub values: Vec<(f64, MetricVal)>,
    // TODO: Include histograms
}

/// The data payload from a Prometheus query response
#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
#[serde(tag = "resultType", content = "result", rename_all = "camelCase")]
pub enum PromData<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    /// Result from a range query (multiple values per series)
    Matrix(Vec<MetricTimeseries<KV>>),
    /// Result from an instant query (single value per series)
    Vector(Vec<MetricValue<KV>>),
}

impl<KV> PromData<KV>
where
    KV: Clone + Debug,
{
    /// Convert to matrix result, returning error if it was a vector
    pub fn into_matrix(self) -> Result<Vec<MetricTimeseries<KV>>, Error> {
        match self {
            Self::Matrix(data) => Ok(data),
            _ => Err(Error::UnexpectedResultType(format!("{self:?}"))),
        }
    }

    /// Convert to vector result, returning error if it was a matrix
    pub fn into_vector(self) -> Result<Vec<MetricValue<KV>>, Error> {
        match self {
            Self::Vector(data) => Ok(data),
            _ => Err(Error::UnexpectedResultType(format!("{self:?}"))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Status {
    Success,
    Error,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "T: DeserializeOwned", rename_all = "camelCase")]
pub(crate) struct PromResponse<T>
where
    T: Clone + Debug,
{
    pub status: Status,
    #[serde(default)]
    pub data: Option<T>,
    #[serde(default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl<T> PromResponse<T>
where
    T: Clone + Debug,
{
    pub fn into_result(self) -> Result<T, Error> {
        for warning in self.warnings {
            warn!("Prometheus API response: {warning}");
        }

        match self.status {
            Status::Success => Ok(self.data.ok_or(Error::MissingData)?),
            Status::Error => Err(Error::API(
                self.error_type.unwrap_or_default(),
                self.error.unwrap_or_default(),
            )),
        }
    }
}

/// Response from /api/v1/alerts endpoint
#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct AlertsResponse<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    /// List of alerts
    pub alerts: Vec<AlertInfo<KV>>,
}

/// Information about a single alert
#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned", rename_all = "camelCase")]
pub struct AlertInfo<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    /// When the alert became active
    pub active_at: DateTime<Utc>,
    /// Alert annotations
    pub annotations: KV,
    /// Alert labels
    pub labels: KV,
    /// Current state of the alert
    pub state: AlertStatus,
    /// The value that triggered the alert
    pub value: String,
}

/// The state of an alert
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    /// Alert condition met but for duration not yet satisfied
    Pending,
    /// Alert is actively firing
    Firing,
    /// Alert has been resolved
    Resolved,
}
