use crate::http::Alert;
use chrono::{FixedOffset, Local, Offset, Utc};
use chrono_tz::Tz;
use conf::Conf;
use prometheus_http_client::{
    AlertInfo, AlertsRequest, ExtractLabels, Labels, LabelsRequest, MetricTimeseries, MetricVal,
    MetricValue, PromRequest, QueryRangeRequest, QueryRequest, SeriesRequest,
    plot::{PlotStyle, PlotThreshold},
};
use rand::RngCore;
use reqwest::Client as ReqwestClient;
use std::{error::Error, str::FromStr, time::Duration};
use tracing::{info, warn};
use walkdir::WalkDir;

#[derive(Clone, Conf, Debug)]
#[conf(at_most_one_of_fields(utc_offset, timezone))]
pub struct PlotterConfig {
    #[conf(long, env)]
    pub prometheus_host: String,
    #[conf(long, env, default_value = "30m", value_parser = conf_extra::parse_duration)]
    pub plot_age_limit: Duration,
    /// Fixed timezone offset for plot timestamps, e.g. "+05:30" or "-08:00".
    /// Mutually exclusive with --timezone.
    #[conf(long, env)]
    pub utc_offset: Option<FixedOffset>,
    /// Timezone name for plot timestamps, e.g. "US/Mountain" or "Europe/Paris".
    /// Mutually exclusive with --utc-offset.
    #[conf(long, env)]
    pub timezone: Option<Tz>,
    /// Stroke width for plot lines
    #[conf(long, env, default_value = "2")]
    pub line_width: u32,
    /// Plot dimensions in pixels (width, height), e.g. "1920,1200" or "(1920, 1200)"
    #[conf(long, env, default_value = "1920,1200", value_parser = parse_dimensions)]
    pub plot_dimensions: (u32, u32),
}

fn parse_dimensions(s: &str) -> Result<(u32, u32), String> {
    let s = s.trim();
    // Strip optional parentheses
    let s = s
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(s);

    let (left, right) = s
        .split_once(',')
        .ok_or_else(|| format!("expected 'width,height', got '{s}'"))?;

    let width = left
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid width: {e}"))?;
    let height = right
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid height: {e}"))?;

    Ok((width, height))
}

pub struct Plotter {
    config: PlotterConfig,
    plot_dir: String,
    reqwest_client: ReqwestClient,
    skip_labels: Vec<String>,
}

impl Plotter {
    pub fn new(config: PlotterConfig) -> Result<Self, &'static str> {
        if config.utc_offset.is_some() && config.timezone.is_some() {
            return Err("Cannot specify both --utc-offset and --timezone");
        }

        let plot_dir = "/tmp".into();
        let reqwest_client = ReqwestClient::new();
        let skip_labels = vec!["job".into(), "instance".into()];

        Ok(Self {
            config,
            plot_dir,
            reqwest_client,
            skip_labels,
        })
    }

    fn create_plot(
        &self,
        matrix: &[MetricTimeseries],
        threshold: Option<PlotThreshold>,
        title: Option<&str>,
    ) -> Result<String, Box<dyn Error>> {
        let filename = format!(
            "{dir}/plot-{num}.png",
            dir = self.plot_dir,
            num = rand::rng().next_u64()
        );
        let utc_offset = if let Some(offset) = self.config.utc_offset {
            offset
        } else if let Some(tz) = self.config.timezone {
            Utc::now().with_timezone(&tz).offset().fix()
        } else {
            *Local::now().offset()
        };
        let mut plot_style = PlotStyle::default()
            .dark_mode()
            .with_line_width(self.config.line_width)
            .with_drawing_area(self.config.plot_dimensions)
            .with_utc_offset(utc_offset)
            .with_skip_labels(self.skip_labels.clone());
        if let Some(title) = title {
            plot_style = plot_style.with_title(title);
        }

        plot_style.plot_timeseries(&filename, matrix, threshold)?;
        Ok(filename)
    }

    pub fn purge_old_plots(&self) {
        for entry in WalkDir::new(&self.plot_dir)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(ext) = entry.path().extension() else {
                continue;
            };
            if ext != "png" && ext != ".png" {
                continue;
            }
            let path = entry.path().display();
            let Ok(metadata) = entry
                .metadata()
                .inspect_err(|err| warn!("Couldn't get metadata for {path}: {err}"))
            else {
                continue;
            };
            let Ok(time) = metadata
                .created()
                .inspect_err(|err| warn!("Couldn't get creation time for {path}: {err}"))
            else {
                continue;
            };
            let Ok(elapsed) = time
                .elapsed()
                .inspect_err(|err| warn!("Elapsed time calculation failed for {path}: {err}"))
            else {
                continue;
            };
            if elapsed > self.config.plot_age_limit
                && let Err(err) = std::fs::remove_file(entry.path())
            {
                warn!("Couldn't remove old png file {path}: {err}");
            }
        }
    }

    pub async fn create_alert_plot(&self, alert: &Alert) -> Result<String, Box<dyn Error>> {
        let expr = alert.parse_expr_from_generator_url()?;

        // Parse expressions like "query < 0.09" or "query < 0.09 and on (instance) up{...}"
        // We look for comparison operators and extract the threshold
        let (base_query, threshold) = parse_alert_expr(&expr)?;

        // Build label selector from alert labels (excluding job/instance)
        let label_selector = build_label_selector(&alert.labels, &self.skip_labels);
        let query = if label_selector.is_empty() {
            base_query.to_owned()
        } else {
            format!("{base_query}{{{label_selector}}}")
        };

        let now = Utc::now();
        let elapsed = now - alert.starts_at;
        // Extend elapsed by 210%, but use at least plot_age_limit (default 30m)
        let lengthen = elapsed
            .checked_mul(31)
            .and_then(|e| e.checked_div(10))
            .ok_or("timedelta overflow")?;
        let min_range = chrono::TimeDelta::from_std(self.config.plot_age_limit)?;
        let range = lengthen.max(min_range);

        info!("Prom range query: {query}");
        let matrix = QueryRangeRequest::builder(query.clone())
            .range(now - range..now)
            .build()
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .into_matrix()?;

        self.create_plot(&matrix, Some(threshold), Some(&query))
    }

    pub async fn create_oneoff_plot(
        &self,
        query: String,
        since: Duration,
    ) -> Result<String, Box<dyn Error>> {
        info!("Prom range query: {query}");
        let matrix = QueryRangeRequest::builder(query.to_owned())
            .since(since)
            .build()
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .into_matrix()?;

        self.create_plot(&matrix, None, Some(&query))
    }

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

        let labels = ExtractLabels::new(vector.iter().map(|mv| &mv.metric), &self.skip_labels);
        let values = vector.into_iter().map(|mv| mv.value).collect();

        Ok((labels, values))
    }

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

    pub async fn alerts(&self) -> Result<Vec<AlertInfo>, Box<dyn Error>> {
        Ok(AlertsRequest {}
            .send_with_client(&self.reqwest_client, &self.config.prometheus_host)
            .await?
            .alerts)
    }
}

/// Build a prometheus label selector string from alert labels, excluding specified labels.
/// E.g., {"asset": "ETH", "job": "ec2"} with skip=["job"] -> `asset="ETH"`
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
