use conf::Conf;
use prometheus_http_client::{
    AlertInfo, AlertsRequest, ExtractLabels, Labels, LabelsRequest, MetricVal, MetricValue,
    PromRequest, QueryRequest, ReqwestClient, SeriesRequest,
};
use std::error::Error;
use tracing::info;

#[cfg(feature = "plot")]
mod plot;
#[cfg(feature = "plot")]
use plot::{PlotConfig, PlotThreshold};

#[cfg(feature = "plot")]
use crate::alertmanager::Alert;
#[cfg(feature = "plot")]
use prometheus_http_client::QueryRangeRequest;
#[cfg(feature = "plot")]
use std::{path::PathBuf, str::FromStr, time::Duration};

/// Configures our prometheus API client
#[derive(Clone, Conf, Debug)]
pub struct PrometheusConfig {
    /// Address of prometheus host. Should start with http and usually indicate port 9090
    #[conf(long, env)]
    pub prometheus_host: String,
    #[cfg(feature = "plot")]
    /// Configuration options for generated plots
    #[conf(flatten, prefix)]
    pub plot: PlotConfig,
}

/// Prometheus HTTP API + plotting if configured
pub struct Prometheus {
    config: PrometheusConfig,
    reqwest_client: ReqwestClient,
}

impl Prometheus {
    /// Create a new Prometheus client object
    pub fn new(config: PrometheusConfig) -> Result<Self, &'static str> {
        let reqwest_client = ReqwestClient::new();

        Ok(Self {
            config,
            reqwest_client,
        })
    }

    /// Evaluate a PromQL query, returning a list of matching current timeseries values and their labels.
    #[allow(clippy::type_complexity)]
    pub async fn oneoff_query(
        &self,
        query: String,
    ) -> Result<(ExtractLabels, Vec<Option<(f64, MetricVal)>>), Box<dyn Error>> {
        info!("Prom query: {query}");
        let vector: Vec<MetricValue> = QueryRequest { query, time: None }
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .into_vector()?;

        let labels = ExtractLabels::new(vector.iter().map(|mv| &mv.metric), &[]);
        let values = vector.into_iter().map(|mv| mv.value).collect();

        Ok((labels, values))
    }

    /// Get the list of all timeseries, and all of their labels, matching given filters.
    pub async fn series(
        &self,
        matches: impl IntoIterator<Item: AsRef<str>>,
    ) -> Result<Vec<Labels>, Box<dyn Error>> {
        Ok(SeriesRequest {
            matches: matches.into_iter().map(|s| s.as_ref().to_owned()).collect(),
        }
        .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
        .await?)
    }

    /// Get the list of all existing labels, matching given filters
    pub async fn labels(
        &self,
        matches: impl IntoIterator<Item: AsRef<str>>,
    ) -> Result<Vec<String>, Box<dyn Error>> {
        Ok(LabelsRequest {
            matches: matches.into_iter().map(|s| s.as_ref().to_owned()).collect(),
        }
        .send_with_client::<()>(&self.reqwest_client, &self.config.prometheus_host)
        .await?)
    }

    /// Get the list of current alerts
    pub async fn alerts(&self) -> Result<Vec<AlertInfo>, Box<dyn Error>> {
        Ok(AlertsRequest {}
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .alerts)
    }
}

#[cfg(feature = "plot")]
impl Prometheus {
    /// Purge old plots from the plot directory
    pub fn purge_old_plots(&self) {
        self.config.plot.purge_old_plots();
    }

    /// Create a new plot corresponding to a given alert. Returns a pathbuf if it is present
    pub async fn create_alert_plot(&self, alert: &Alert) -> Result<PathBuf, Box<dyn Error>> {
        use chrono::{TimeDelta, Utc};

        let expr = alert.parse_expr_from_generator_url()?;

        // Parse expressions like "query < 0.09" or "query < 0.09 and on (instance) up{...}"
        // We look for comparison operators and extract the threshold
        let (base_query, threshold) = parse_alert_expr(&expr)?;

        // Build label selector from alert labels (excluding job/instance)
        let label_selector = build_label_selector(&alert.labels, &[]);
        let query = if label_selector.is_empty() {
            base_query.to_owned()
        } else {
            format!("{base_query}{{{label_selector}}}")
        };

        let now = Utc::now();
        let elapsed = now - alert.starts_at;
        // Extend elapsed by 110%, but use at least 30m
        let lengthen = elapsed
            .checked_mul(21)
            .and_then(|e| e.checked_div(10))
            .ok_or("timedelta overflow")?;
        let min_range = TimeDelta::minutes(30);
        let range = lengthen.max(min_range);

        info!("Prom range query: {query}");
        let matrix = QueryRangeRequest::builder(query.clone())
            .range(now - range..now)
            .build()
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .into_matrix()?;

        self.config
            .plot
            .create_plot(&matrix, Some(threshold), Some(&query))
    }

    /// Create a plot for a given query and timeframe
    pub async fn create_oneoff_plot(
        &self,
        query: String,
        since: Duration,
    ) -> Result<PathBuf, Box<dyn Error>> {
        info!("Prom range query: {query}");
        let matrix = QueryRangeRequest::builder(query.to_owned())
            .since(since)
            .build()
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .into_matrix()?;

        self.config.plot.create_plot(&matrix, None, Some(&query))
    }
}

/// Build a prometheus label selector string from alert labels, excluding specified labels.
/// E.g., {"asset": "ETH", "job": "ec2"} with skip=["job"] -> `asset="ETH"`
#[cfg(feature = "plot")]
fn build_label_selector(
    labels: &std::collections::BTreeMap<String, String>,
    skip: &[String],
) -> String {
    labels
        .iter()
        .filter(|(k, _)| !skip.contains(k) && *k != "alertname")
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse an alert expression to extract the base query and threshold.
///
/// Handles expressions like:
/// - "query < 0.09"
/// - "query > 100"
/// - "rate(tick_successes[5m]) < 0.09 and on (instance) up{job=\"ec2\"}"
///
/// Returns (base_query, threshold) where base_query is the part before the comparator.
#[cfg(feature = "plot")]
fn parse_alert_expr(expr: &str) -> Result<(String, PlotThreshold), Box<dyn std::error::Error>> {
    // Look for comparison operators with surrounding spaces
    for comparator in [" < ", " > "] {
        if let Some(pos) = expr.find(comparator) {
            let base_query = expr[..pos].to_owned();
            let after_comparator = &expr[pos + comparator.len()..];

            // The threshold is the first space-delimited token after the comparator
            let numeric = after_comparator
                .split_whitespace()
                .next()
                .ok_or("no numeric after comparator")?;

            let limit = f64::from_str(numeric)?;
            let threshold = if comparator == " < " {
                PlotThreshold::LessThan(limit)
            } else {
                PlotThreshold::GreaterThan(limit)
            };

            return Ok((base_query, threshold));
        }
    }

    Err("no comparator (< or >) found in expression".into())
}
