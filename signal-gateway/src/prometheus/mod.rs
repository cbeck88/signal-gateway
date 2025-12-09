use crate::claude::{Tool, ToolExecutor, ToolResult};
use async_trait::async_trait;
use conf::Conf;
use prometheus_http_client::{
    AlertInfo, AlertsRequest, ExtractLabels, Labels, LabelsRequest, MetricVal, MetricValue,
    PromRequest, QueryRequest, ReqwestClient, SeriesRequest,
};
use std::error::Error;
use std::fmt::Write;
use tracing::info;

type BoxError = Box<dyn Error + Send + Sync>;

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
#[conf(serde)]
pub struct PrometheusConfig {
    /// URL of the Prometheus server query API (e.g., `http://localhost:9090`).
    #[conf(long, env)]
    pub prometheus_url: String,
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
    ) -> Result<(ExtractLabels, Vec<Option<(f64, MetricVal)>>), BoxError> {
        info!("Prom query: {query}");
        let vector: Vec<MetricValue> = QueryRequest { query, time: None }
            .send_with_client(&self.reqwest_client, &self.config.prometheus_url)
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
    ) -> Result<Vec<Labels>, BoxError> {
        Ok(SeriesRequest {
            matches: matches.into_iter().map(|s| s.as_ref().to_owned()).collect(),
        }
        .send_with_client(&self.reqwest_client, &self.config.prometheus_url)
        .await?)
    }

    /// Get the list of all existing labels, matching given filters
    pub async fn labels(
        &self,
        matches: impl IntoIterator<Item: AsRef<str>>,
    ) -> Result<Vec<String>, BoxError> {
        Ok(LabelsRequest {
            matches: matches.into_iter().map(|s| s.as_ref().to_owned()).collect(),
        }
        .send_with_client::<()>(&self.reqwest_client, &self.config.prometheus_url)
        .await?)
    }

    /// Get the list of current alerts
    pub async fn alerts(&self) -> Result<Vec<AlertInfo>, BoxError> {
        Ok(AlertsRequest {}
            .send_with_client(&self.reqwest_client, &self.config.prometheus_url)
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

    /// Create a new plot corresponding to a given alert. Returns a pathbuf if it is present.
    ///
    /// If `add_label_selector` is true, appends alert labels as a label selector to the query.
    /// This only works for simple metric queries, not for function calls like `rate(...)`.
    pub async fn create_alert_plot(
        &self,
        alert: &Alert,
        add_label_selector: bool,
    ) -> Result<PathBuf, BoxError> {
        use chrono::{TimeDelta, Utc};

        let expr = alert.parse_expr_from_generator_url()?;

        // Parse expressions like "query < 0.09" or "query < 0.09 and on (instance) up{...}"
        // We look for comparison operators and extract the threshold
        let (base_query, threshold) = parse_alert_expr(&expr)?;

        // Optionally add label selector from alert labels
        let query = if add_label_selector {
            let label_selector = build_label_selector(&alert.labels, &[]);
            if label_selector.is_empty() {
                base_query
            } else {
                format!("{base_query}{{{label_selector}}}")
            }
        } else {
            base_query
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
            .send_with_client(&self.reqwest_client, &self.config.prometheus_url)
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
    ) -> Result<PathBuf, BoxError> {
        info!("Prom range query: {query}");
        let matrix = QueryRangeRequest::builder(query.to_owned())
            .since(since)
            .build()
            .send_with_client(&self.reqwest_client, &self.config.prometheus_url)
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
fn parse_alert_expr(expr: &str) -> Result<(String, PlotThreshold), BoxError> {
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

// Tool definitions for Claude API integration

fn query_tool() -> Tool {
    Tool {
        name: "prometheus_query",
        description: "Query Prometheus for current metric values using PromQL. Returns the current value of metrics matching the query expression.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A PromQL query expression (e.g., 'up', 'rate(http_requests_total[5m])', 'node_memory_MemFree_bytes / node_memory_MemTotal_bytes')"
                }
            },
            "required": ["query"]
        }),
    }
}

fn series_tool() -> Tool {
    Tool {
        name: "prometheus_series",
        description: "List all time series (metrics) matching the given label matchers. Returns metric names with all their label key-value pairs.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "matchers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Label matchers to filter series (e.g., ['__name__=~\"http_.*\"', 'job=\"api\"']). Use __name__ to match metric names."
                }
            },
            "required": ["matchers"]
        }),
    }
}

fn labels_tool() -> Tool {
    Tool {
        name: "prometheus_labels",
        description: "List all label names that exist in Prometheus, optionally filtered by series matchers.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "matchers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional label matchers to filter which series to consider (e.g., ['job=\"api\"']). If empty, returns all label names."
                }
            },
            "required": []
        }),
    }
}

fn alerts_tool() -> Tool {
    Tool {
        name: "prometheus_alerts",
        description: "List all current alerts from Prometheus, including their state (pending/firing), labels, and annotations.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn plot_tool() -> Tool {
    Tool {
        name: "prometheus_plot",
        description: "Create a plot/graph of a Prometheus metric over time. Returns the plot as an image attachment.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A PromQL query expression to plot (e.g., 'rate(http_requests_total[5m])')"
                },
                "range": {
                    "type": "string",
                    "description": "Time range to plot, as a duration string (e.g., '1h', '30m', '2d'). Defaults to '1h'."
                }
            },
            "required": ["query"]
        }),
    }
}

#[async_trait]
impl ToolExecutor for Prometheus {
    fn tools(&self) -> Vec<Tool> {
        vec![
            query_tool(),
            series_tool(),
            labels_tool(),
            alerts_tool(),
            plot_tool(),
        ]
    }

    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<ToolResult, String> {
        match name {
            "prometheus_query" => {
                let query = input
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing 'query' parameter".to_owned())?;

                match self.oneoff_query(query.to_owned()).await {
                    Ok((labels, values)) => {
                        let mut result = format!(
                            "Query: {}\nMetric: {} (common labels: {:?})\n",
                            query, labels.name, labels.common_labels
                        );
                        for (sl, val) in labels.specific_labels.iter().zip(values.iter()) {
                            let value_str = val
                                .as_ref()
                                .map(|(_, v)| v.to_string())
                                .unwrap_or_else(|| "-".to_owned());
                            writeln!(&mut result, "  {:?} = {}", sl, value_str).unwrap();
                        }
                        Ok(result.into())
                    }
                    Err(err) => Err(format!("prometheus query failed: {err}")),
                }
            }
            "prometheus_series" => {
                let matchers: Vec<String> = input
                    .get("matchers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default();

                match self.series(&matchers).await {
                    Ok(series) => {
                        let mut result = format!("Found {} series:\n", series.len());
                        for labels in series.iter().take(100) {
                            writeln!(&mut result, "  {:?}", labels).unwrap();
                        }
                        if series.len() > 100 {
                            result.push_str(&format!("  ... and {} more\n", series.len() - 100));
                        }
                        Ok(result.into())
                    }
                    Err(err) => Err(format!("prometheus series failed: {err}")),
                }
            }
            "prometheus_labels" => {
                let matchers: Vec<String> = input
                    .get("matchers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default();

                match self.labels(&matchers).await {
                    Ok(labels) => {
                        let mut result = format!("Found {} labels:\n", labels.len());
                        for label in &labels {
                            writeln!(&mut result, "  {}", label).unwrap();
                        }
                        Ok(result.into())
                    }
                    Err(err) => Err(format!("prometheus labels failed: {err}")),
                }
            }
            "prometheus_alerts" => match self.alerts().await {
                Ok(alerts) => {
                    if alerts.is_empty() {
                        return Ok("No active alerts".into());
                    }
                    let mut result = format!("Found {} alerts:\n", alerts.len());
                    for alert in &alerts {
                        writeln!(
                            &mut result,
                            "  [{:?}] {:?} - {:?}",
                            alert.state, alert.labels, alert.annotations
                        )
                        .unwrap();
                    }
                    Ok(result.into())
                }
                Err(err) => Err(format!("prometheus alerts failed: {err}")),
            },
            "prometheus_plot" => {
                #[cfg(feature = "plot")]
                {
                    let query = input
                        .get("query")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "missing 'query' parameter".to_owned())?;

                    // Parse range, default to 1h
                    let range_str = input.get("range").and_then(|v| v.as_str()).unwrap_or("1h");
                    let range = conf_extra::parse_duration(range_str)
                        .map_err(|e| format!("invalid range '{}': {}", range_str, e))?;

                    match self.create_oneoff_plot(query.to_owned(), range).await {
                        Ok(path) => Ok(ToolResult::with_attachment(
                            format!("Plot created: {}", path.display()),
                            path,
                        )),
                        Err(err) => Err(format!("prometheus plot failed: {err}")),
                    }
                }
                #[cfg(not(feature = "plot"))]
                {
                    Err("prometheus_plot requires the 'plot' feature to be enabled".to_owned())
                }
            }
            _ => Err(format!("unknown tool: {name}")),
        }
    }
}
