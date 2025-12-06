use super::BoxError;
use chrono::{FixedOffset, Local, Offset, Utc};
use chrono_tz::Tz;
use conf::Conf;
use rand::RngCore;
use std::{path::PathBuf, time::Duration};
use tracing::{error, warn};
use walkdir::WalkDir;

pub use prometheus_http_client::{
    MetricTimeseries,
    plot::{PlotStyle, PlotThreshold},
};

/// Configuration for plots generated from prometheus data
#[derive(Clone, Conf, Debug)]
#[conf(serde, at_most_one_of_fields(utc_offset, timezone))]
pub struct PlotConfig {
    /// Working directory for temporary plot files
    #[conf(long, env, default_value = "/tmp")]
    dir: PathBuf,
    /// How long before temporary plot files are removed from the working directory
    #[conf(long, env, default_value = "30m", value_parser = conf_extra::parse_duration)]
    age_limit: Duration,
    /// Fixed timezone offset for plot timestamps, e.g. "+05:30" or "-08:00".
    /// Mutually exclusive with --timezone.
    #[conf(long, env, serde(use_value_parser))]
    utc_offset: Option<FixedOffset>,
    /// Timezone name for plot timestamps, e.g. "US/Mountain" or "Europe/Paris".
    /// Mutually exclusive with --utc-offset.
    #[conf(long, env, serde(use_value_parser))]
    timezone: Option<Tz>,
    /// Stroke width for plot lines
    #[conf(long, env, default_value = "2")]
    line_width: u32,
    /// Plot dimensions in pixels (width, height), e.g. "1920,1200" or "(1920, 1200)"
    #[conf(long, env, default_value = "1920,1200", value_parser = parse_dimensions)]
    dimensions: (u32, u32),
    /// Don't show these labels in the plot title or legend
    #[conf(long, env, default_value = "[\"job\", \"instance\"]", value_parser = serde_json::from_str)]
    skip_labels: Vec<String>,
}

impl PlotConfig {
    pub fn create_plot(
        &self,
        matrix: &[MetricTimeseries],
        threshold: Option<PlotThreshold>,
        title: Option<&str>,
    ) -> Result<PathBuf, BoxError> {
        let mut filename = self.dir.clone();
        filename.push(format!("plot-{}", rand::rng().next_u64()));
        filename.set_extension("png");

        let utc_offset = if let Some(offset) = self.utc_offset {
            offset
        } else if let Some(tz) = self.timezone {
            Utc::now().with_timezone(&tz).offset().fix()
        } else {
            *Local::now().offset()
        };
        let mut plot_style = PlotStyle::default()
            .dark_mode()
            .with_line_width(self.line_width)
            .with_drawing_area(self.dimensions)
            .with_utc_offset(utc_offset)
            .with_skip_labels(self.skip_labels.clone());
        if let Some(title) = title {
            plot_style = plot_style.with_title(title);
        }

        plot_style.plot_timeseries(&filename, matrix, threshold)?;
        Ok(filename)
    }

    pub fn purge_old_plots(&self) {
        for entry in WalkDir::new(&self.dir)
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
            if elapsed > self.age_limit
                && let Err(err) = std::fs::remove_file(entry.path())
            {
                error!("Couldn't remove old png file {path}: {err}");
            }
        }
    }
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
