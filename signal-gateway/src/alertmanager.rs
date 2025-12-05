//! Schema for the alertmanager http POST requests that are sent to us

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Resolved,
    Firing,
}

impl Status {
    pub fn symbol(&self) -> char {
        match self {
            Status::Firing => '🔴',
            Status::Resolved => '🟢',
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertPost {
    // should be 4.0
    pub version: String,
    pub group_key: String,
    pub status: Status,
    pub receiver: String,
    #[serde(default)]
    pub group_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub common_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub common_annotations: BTreeMap<String, String>,
    #[serde(alias = "externalURL")]
    pub external_url: String,
    pub alerts: Vec<Alert>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub status: Status,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[serde(alias = "generatorURL")]
    pub generator_url: String,
    pub fingerprint: String,
}

impl Alert {
    pub fn parse_expr_from_generator_url(&self) -> Result<String, String> {
        let url = Url::parse(&self.generator_url).map_err(|err| err.to_string())?;

        for (k, v) in url.query_pairs() {
            if k == "g0.expr" {
                return Ok(v.into_owned());
            }
        }

        Err("Couldn't find g0.expr".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alert_message_parsing() {
        let text = r#"{"receiver":"notify-admin","status":"firing","alerts":[{"status":"firing","labels":{"alertname":"Low tick success rate","instance":"172.31.5.8:9000","job":"ec2"},"annotations":{"summary":"Low tick success rate"},"startsAt":"2025-11-07T04:21:46.17Z","endsAt":"0001-01-01T00:00:00Z","generatorURL":"http://ip-172-31-10-138.eu-west-3.compute.internal:9090/graph?g0.expr=rate%28tick_successes%5B5m%5D%29+%3C+0.9\u0026g0.tab=1","fingerprint":"543b6a7a3042ae2c"},{"status":"firing","labels":{"alertname":"Long tail tick times","instance":"172.31.5.8:9000","job":"ec2","quantile":"0.99"},"annotations":{"summary":"Long tail tick times"},"startsAt":"2025-11-07T04:50:01.17Z","endsAt":"0001-01-01T00:00:00Z","generatorURL":"http://ip-172-31-10-138.eu-west-3.compute.internal:9090/graph?g0.expr=tick_time%7Bquantile%3D%220.99%22%7D+%3E+0.8\u0026g0.tab=1","fingerprint":"97130d38ef0ff0a4"}],"groupLabels":{},"commonLabels":{"instance":"172.31.5.8:9000","job":"ec2"},"commonAnnotations":{},"externalURL":"http://ip-172-31-10-138.eu-west-3.compute.internal:9093","version":"4","groupKey":"{}:{}","truncatedAlerts":0}"#;

        let msg: AlertPost = serde_json::from_str(text).unwrap();

        assert_eq!(&msg.receiver, "notify-admin");
        assert_eq!(msg.alerts.len(), 2);
        assert_eq!(
            msg.alerts[0].annotations.get("summary").unwrap(),
            "Low tick success rate"
        );

        let expr = msg.alerts[1].parse_expr_from_generator_url().unwrap();
        assert_eq!(expr, r#"tick_time{quantile="0.99"} > 0.8"#);
    }
}
