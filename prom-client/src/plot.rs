//! Make a simple plot of time-series data from prometheus

use crate::{ExtractLabels, MetricTimeseries};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use plotters::prelude::*;
use std::{
    borrow::Borrow,
    error::Error,
    fmt::{Debug, Display, Write},
    ops::Range,
    path::Path,
};

/// Styling options for the plot
pub struct PlotStyle {
    /// The pixel size of the plot
    pub drawing_area: (u32, u32),
    /// The background color
    pub background: RGBAColor,
    /// The grid color
    pub grid: RGBAColor,
    /// The axis color
    pub axis: RGBAColor,
    /// The text color
    pub text_color: RGBAColor,
    /// The text font
    pub text_font: String,
    /// The text size
    pub text_size: u32,
    /// The caption size
    pub caption_size: u32,
    /// The colors to use for lines. If there are more lines than this, then colors will be repeated.
    pub data_colors: Vec<RGBAColor>,
    /// The colors to use for a "threshold" such as used in a PromQL alerting rule
    pub threshold_color: RGBAColor,
    /// Labels to skip rendering of
    pub skip_labels: Vec<String>,
    /// UTC offset to use when labelling the timestamps being plotted
    pub utc_offset_hours: i32,
    /// Optional title override - used when prometheus aggregations remove __name__
    pub title: Option<String>,
}

impl Default for PlotStyle {
    fn default() -> Self {
        Self {
            drawing_area: (1920, 1200),
            background: WHITE.into(),
            grid: RGBAColor(100, 100, 100, 0.5),
            axis: BLACK.into(),
            text_color: BLACK.into(),
            text_font: "sans-serif".into(),
            text_size: 18,
            caption_size: 36,
            data_colors: [
                GREEN,
                BLUE,
                full_palette::ORANGE,
                YELLOW,
                MAGENTA,
                full_palette::TEAL,
                full_palette::PURPLE,
            ]
            .iter()
            .cloned()
            .map(Into::into)
            .collect(),
            threshold_color: RED.mix(0.2),
            skip_labels: vec!["job".into(), "instance".into()],
            utc_offset_hours: 0,
            title: None,
        }
    }
}

impl PlotStyle {
    /// Set the drawing_area
    pub fn with_drawing_area(mut self, drawing_area: impl Into<(u32, u32)>) -> Self {
        self.drawing_area = drawing_area.into();
        self
    }

    /// Override the title of the plot
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the UTC offset (timezone) used in the plot, in hours
    pub fn with_utc_offset(mut self, offset: i32) -> Self {
        self.utc_offset_hours = offset;
        self
    }
}

/// A shaded region appearing on the plot to indicate values that would trigger an alert
pub enum PlotThreshold {
    /// Shade values greater than this threshold
    GreaterThan(f64),
    /// Shade values less than this threshold
    LessThan(f64),
}

impl PlotStyle {
    /// Use a dark color scheme for the plot
    pub fn dark_mode(mut self) -> Self {
        self.background = BLACK.into();
        self.grid = RGBAColor(100, 100, 100, 0.5);
        self.axis = WHITE.into();
        self.text_color = WHITE.into();
        self
    }

    /// Plot a collection of metric timeseries data from prometheus, and possibly a "threshold" defined in an alert.
    /// Write the result to a path. The file extension of the path will determine the format, e.g. png, gif, etc.
    pub fn plot_timeseries<KV, K, V>(
        &self,
        path: impl AsRef<Path>,
        mts: &[MetricTimeseries<KV>],
        plot_threshold: Option<PlotThreshold>,
    ) -> Result<(), Box<dyn Error>>
    where
        KV: Clone + Debug,
        K: Display,
        V: Display,
        for<'a> &'a KV: IntoIterator<Item = (&'a K, &'a V)>,
    {
        // Prepare to plot by scanning the data, finding x and y bounds, common labels, etc.
        let ExtractLabels {
            name,
            common_labels,
            specific_labels,
        } = ExtractLabels::new(mts.iter().map(|mts| &mts.metric), &self.skip_labels);
        let PreparedPlot {
            x_range,
            y_range,
            ts,
        } = PreparedPlot::prepare(mts)?;

        // Figure out the caption for formatting style for date-times
        // Use title override if provided, otherwise use extracted metric name
        // Only append common_labels if no title override (since title likely already has labels)
        let mut caption = if let Some(title) = &self.title {
            title.clone()
        } else {
            let mut c = name;
            if !common_labels.is_empty() {
                write!(&mut c, " {common_labels:?}")?;
            }
            c
        };

        // Format date-times differently depending on the range of date-times being displayed.
        //
        // If they are all on the same day, then omit the day, and put it in the caption instead
        let timezone =
            FixedOffset::east_opt(3600 * self.utc_offset_hours).ok_or("invalid timezone")?;
        let start_date_naive = x_range.start.with_timezone(&timezone).date_naive();
        let date_format_str =
            if start_date_naive == x_range.end.with_timezone(&timezone).date_naive() {
                write!(&mut caption, " {start_date_naive}")?;
                "%H:%M:%S"
            } else {
                "%m/%d %H:%M:%S"
            };

        // Add timezone offset to the caption
        write!(&mut caption, " UTC{o:+}", o = self.utc_offset_hours)?;

        // Actually start writing the file
        let root_area = BitMapBackend::new(&path, self.drawing_area).into_drawing_area();
        root_area.fill(&self.background)?;

        let mut ctx = ChartBuilder::on(&root_area)
            .set_label_area_size(LabelAreaPosition::Left, 100)
            .set_label_area_size(LabelAreaPosition::Bottom, 40)
            .caption(
                caption,
                (self.text_font.as_str(), self.caption_size, &self.text_color)
                    .into_text_style(&root_area),
            )
            .build_cartesian_2d(x_range.clone(), y_range.clone())?;

        let text_style =
            (self.text_font.as_str(), self.text_size, &self.text_color).into_text_style(&root_area);

        ctx.configure_mesh()
            .light_line_style(self.grid) // Dark gray grid lines
            .axis_style(self.axis) // White axis lines
            .bold_line_style(self.axis) // White bold lines
            .label_style(text_style.clone())
            .x_label_formatter(&|x| {
                x.with_timezone(&timezone)
                    .format(date_format_str)
                    .to_string()
            })
            .draw()?;

        if let Some(threshold) = plot_threshold {
            let (limit, baseline) = match threshold {
                PlotThreshold::GreaterThan(limit) => (limit, y_range.end),
                PlotThreshold::LessThan(limit) => (limit, y_range.start),
            };
            ctx.draw_series(AreaSeries::new(
                [(x_range.start, limit), (x_range.end, limit)],
                baseline,
                self.threshold_color,
            ))?;
        }

        // Only show legend if there are multiple series (single series doesn't need a legend)
        let show_legend = specific_labels.len() > 1;

        for (idx, (mut metric, vals)) in specific_labels.into_iter().zip(ts.into_iter()).enumerate()
        {
            let color = &self.data_colors[idx % self.data_colors.len()];

            let name = metric.remove("__name__").unwrap_or_default();
            let label = format!("{name} {metric:?}");

            let series = ctx.draw_series(LineSeries::new(vals, color))?;
            if show_legend {
                series
                    .label(label)
                    .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color));
            }
        }

        if show_legend {
            ctx.configure_series_labels()
                .position(SeriesLabelPosition::LowerLeft)
                .border_style(self.axis)
                .background_style(self.background.mix(0.8))
                .label_font(text_style)
                .draw()?;
        }

        // Signal any errors that occurred when writing the file
        // https://github.com/plotters-rs/plotters?tab=readme-ov-file#faq-list
        root_area.present()?;

        Ok(())
    }
}

struct PreparedPlot {
    x_range: Range<DateTime<Utc>>,
    y_range: Range<f64>,
    ts: Vec<Vec<(DateTime<Utc>, f64)>>,
}

impl PreparedPlot {
    fn prepare<KV>(data: &[impl Borrow<MetricTimeseries<KV>>]) -> Result<Self, &'static str>
    where
        KV: Clone + Debug,
    {
        let mut x_range = None;
        let mut y_range = None;

        let ts: Vec<Vec<(_, f64)>> = data
            .iter()
            .map(|mts| {
                let mts = mts.borrow();

                mts.values
                    .iter()
                    .filter_map(|(k, v)| {
                        let x = f64_to_datetime(k)?;
                        let y = *v.as_ref();
                        extend_range(&mut x_range, &x);
                        extend_range(&mut y_range, &y);

                        Some((x, y))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        let x_range = x_range.ok_or("No data")?;
        let y_range = y_range.ok_or("No data")?;

        Ok(Self {
            x_range,
            y_range,
            ts,
        })
    }
}

fn f64_to_datetime(t: &f64) -> Option<DateTime<Utc>> {
    let seconds = t.trunc() as i64;
    let nanoseconds = (t.fract() * 1_000_000_000.0) as u32;

    Utc.timestamp_opt(seconds, nanoseconds).single()
}

fn extend_range<T: PartialOrd + Clone>(range: &mut Option<Range<T>>, val: &T) {
    if let Some(range) = range.as_mut() {
        if range.start > *val {
            range.start = val.clone();
        } else if range.end < *val {
            range.end = val.clone();
        }
    } else {
        *range = Some(val.clone()..val.clone())
    }
}
