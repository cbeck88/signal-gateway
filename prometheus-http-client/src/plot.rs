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
#[non_exhaustive]
pub struct PlotStyle {
    // The pixel size of the plot
    drawing_area: (u32, u32),
    // The background color
    background: RGBAColor,
    // The grid color
    grid: RGBAColor,
    // The axis color
    axis: RGBAColor,
    // The text color
    text_color: RGBAColor,
    // The text font
    text_font: String,
    // The text size
    text_size: u32,
    // The caption size
    caption_size: u32,
    // The colors to use for lines. If there are more lines than this, then colors will be repeated.
    data_colors: Vec<RGBAColor>,
    // The colors to use for a "threshold" such as used in a PromQL alerting rule
    threshold_color: RGBAColor,
    // Labels to skip rendering of
    skip_labels: Vec<String>,
    // Timezone offset to use when labelling the timestamps being plotted
    utc_offset: FixedOffset,
    // Optional title override - used when prometheus aggregations remove __name__
    title: Option<String>,
    // The stroke width for data lines (default: 1)
    line_width: u32,
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
            utc_offset: FixedOffset::east_opt(0).unwrap(),
            title: None,
            line_width: 1,
        }
    }
}

impl PlotStyle {
    /// Set the pixel dimensions of the plot (width, height). Default is 1920x1200.
    pub fn with_drawing_area(mut self, drawing_area: impl Into<(u32, u32)>) -> Self {
        self.drawing_area = drawing_area.into();
        self
    }

    /// Override the title of the plot. By default, the title is derived from the metric name
    /// and common labels.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the timezone offset for timestamp labels. Default is UTC.
    /// Supports fractional-hour timezones like UTC+5:30.
    pub fn with_utc_offset(mut self, offset: FixedOffset) -> Self {
        self.utc_offset = offset;
        self
    }

    /// Set the stroke width for data lines in pixels. Default is 1.
    pub fn with_line_width(mut self, width: u32) -> Self {
        self.line_width = width;
        self
    }

    /// Set label names to exclude from the legend (e.g., "job", "instance").
    pub fn with_skip_labels(mut self, labels: Vec<String>) -> Self {
        self.skip_labels = labels;
        self
    }

    /// Set the background color of the plot. Default is white.
    pub fn with_background(mut self, color: impl Into<RGBAColor>) -> Self {
        self.background = color.into();
        self
    }

    /// Set the color of the grid lines. Default is semi-transparent gray.
    pub fn with_grid_color(mut self, color: impl Into<RGBAColor>) -> Self {
        self.grid = color.into();
        self
    }

    /// Set the color of the axis lines. Default is black.
    pub fn with_axis_color(mut self, color: impl Into<RGBAColor>) -> Self {
        self.axis = color.into();
        self
    }

    /// Set the color of axis labels and legend text. Default is black.
    pub fn with_text_color(mut self, color: impl Into<RGBAColor>) -> Self {
        self.text_color = color.into();
        self
    }

    /// Set the font family for text. Default is "sans-serif".
    pub fn with_text_font(mut self, font: impl Into<String>) -> Self {
        self.text_font = font.into();
        self
    }

    /// Set the font size for axis labels and legend in pixels. Default is 18.
    pub fn with_text_size(mut self, size: u32) -> Self {
        self.text_size = size;
        self
    }

    /// Set the font size for the plot title/caption in pixels. Default is 36.
    pub fn with_caption_size(mut self, size: u32) -> Self {
        self.caption_size = size;
        self
    }

    /// Set the colors to cycle through for data lines. If there are more series than colors,
    /// colors will repeat.
    pub fn with_data_colors(mut self, colors: Vec<RGBAColor>) -> Self {
        self.data_colors = colors;
        self
    }

    /// Set the color used to shade the threshold region in alert plots. Default is
    /// semi-transparent red.
    pub fn with_threshold_color(mut self, color: impl Into<RGBAColor>) -> Self {
        self.threshold_color = color.into();
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
    /// Switch to a dark color scheme: black background with white text and axes.
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
    ) -> Result<(), Box<dyn Error + Send + Sync>>
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
        let start_date_naive = x_range.start.with_timezone(&self.utc_offset).date_naive();
        let date_format_str =
            if start_date_naive == x_range.end.with_timezone(&self.utc_offset).date_naive() {
                write!(&mut caption, " {start_date_naive}")?;
                "%H:%M:%S"
            } else {
                "%m/%d %H:%M:%S"
            };

        // Add timezone offset to the caption (FixedOffset displays as +HH:MM or -HH:MM)
        write!(&mut caption, " UTC{}", self.utc_offset)?;

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
                x.with_timezone(&self.utc_offset)
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
            let style = ShapeStyle::from(color).stroke_width(self.line_width);

            let name = metric.remove("__name__").unwrap_or_default();
            let label = format!("{name} {metric:?}");

            let series = ctx.draw_series(LineSeries::new(vals, style))?;
            if show_legend {
                series
                    .label(label)
                    .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], style));
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
