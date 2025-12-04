// The prometheus http interface (at port 9090) renders graphs in JS and gets the raw data using API requests like this:
//
// GET http://localhost:9090/api/v1/query_range?query=tick_time{quantile="0.99"}&step=14&start=1762534433.802&end=1762538033.802
//
// Response is:
//
// {
//   status: "success"
//   data: {
//     resultType: "matrix",
//     result: [
//       {
//         metric: { __name__: "tick_time", instance: "x.y.z.w", job: "ec2", .. },
//         values: [
//            [ 1762534433.802, "1.8974293514080933" ],
//            [ 1762534447.802, "2.029724353457351" ],
//            ..
//         ]
//       }
//     ]
//   }
// }
//
// For more detail see:
// https://prometheus.io/docs/prometheus/latest/querying/api/

use crate::Error;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashMap, fmt::Debug};
use tracing::warn;

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct MetricValue<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    pub metric: KV,
    #[serde(default)]
    pub value: Option<(f64, Decimal)>,
    // TODO: Include histograms
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct MetricTimeseries<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    pub metric: KV,
    #[serde(default)]
    pub values: Vec<(f64, Decimal)>,
    // TODO: Include histograms
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
#[serde(tag = "resultType", content = "result", rename_all = "camelCase")]
pub enum PromData<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    Matrix(Vec<MetricTimeseries<KV>>),
    Vector(Vec<MetricValue<KV>>),
}

impl<KV> PromData<KV>
where
    KV: Clone + Debug,
{
    pub fn into_matrix(self) -> Result<Vec<MetricTimeseries<KV>>, Error> {
        match self {
            Self::Matrix(data) => Ok(data),
            _ => Err(Error::UnexpectedResultType(format!("{self:?}"))),
        }
    }

    pub fn into_vector(self) -> Result<Vec<MetricValue<KV>>, Error> {
        match self {
            Self::Vector(data) => Ok(data),
            _ => Err(Error::UnexpectedResultType(format!("{self:?}"))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Success,
    Error,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "T: DeserializeOwned", rename_all = "camelCase")]
pub struct PromResponse<T>
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

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned")]
pub struct AlertsResponse<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    pub alerts: Vec<AlertInfo<KV>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(bound = "KV: DeserializeOwned", rename_all = "camelCase")]
pub struct AlertInfo<KV = HashMap<String, String>>
where
    KV: Clone + Debug,
{
    pub active_at: DateTime<Utc>,
    pub annotations: KV,
    pub labels: KV,
    pub state: AlertStatus,
    pub value: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    Pending,
    Firing,
    Resolved,
}
