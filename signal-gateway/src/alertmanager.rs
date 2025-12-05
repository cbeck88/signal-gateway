//! Schema for the Alertmanager HTTP POST webhook requests.
//!
//! See <https://prometheus.io/docs/alerting/latest/configuration/#webhook_config>

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

/// Alert status indicating whether an alert is firing or resolved.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The alert condition is no longer true.
    Resolved,
    /// The alert condition is currently true.
    Firing,
}

impl Status {
    /// Returns an emoji symbol representing the status.
    pub fn symbol(&self) -> char {
        match self {
            Status::Firing => '🔴',
            Status::Resolved => '🟢',
        }
    }
}

/// The top-level webhook payload from Alertmanager.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertPost {
    /// Alertmanager API version (should be "4").
    pub version: String,
    /// Key used to group alerts together.
    pub group_key: String,
    /// Overall status of the alert group.
    pub status: Status,
    /// Name of the receiver that triggered this webhook.
    pub receiver: String,
    /// Labels used to group the alerts.
    #[serde(default)]
    pub group_labels: BTreeMap<String, String>,
    /// Labels common to all alerts in this group.
    #[serde(default)]
    pub common_labels: BTreeMap<String, String>,
    /// Annotations common to all alerts in this group.
    #[serde(default)]
    pub common_annotations: BTreeMap<String, String>,
    /// URL of the Alertmanager instance.
    #[serde(alias = "externalURL")]
    pub external_url: String,
    /// List of alerts in this notification.
    pub alerts: Vec<Alert>,
}

/// An individual alert from Alertmanager.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// Status of this specific alert.
    pub status: Status,
    /// Labels identifying the alert.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Annotations providing additional information.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Time when the alert started firing.
    pub starts_at: DateTime<Utc>,
    /// Time when the alert was resolved (zero value if still firing).
    pub ends_at: DateTime<Utc>,
    /// URL to the Prometheus graph for this alert's expression.
    #[serde(alias = "generatorURL")]
    pub generator_url: String,
    /// Unique identifier for this alert.
    pub fingerprint: String,
}

impl Alert {
    /// Parse the PromQL expression from the generator URL.
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
