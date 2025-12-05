//! Builder for QueryRangeRequest

use super::QueryRangeRequest;
use chrono::{DateTime, TimeDelta, Utc};
use std::{ops::Range, time::Duration};

/// Builder for constructing a QueryRangeRequest
pub struct QueryRangeRequestBuilder {
    query: String,
    range: Option<Range<DateTime<Utc>>>,
    step: Option<Duration>,
    count: usize,
}

impl QueryRangeRequestBuilder {
    /// Create a new builder with the given PromQL query
    pub fn new(query: String) -> Self {
        Self {
            query,
            range: None,
            step: None,
            count: 256,
        }
    }

    /// Set the time range for the query
    pub fn range(mut self, range: Range<DateTime<Utc>>) -> Self {
        if self.range.is_some() {
            panic!("already set range: {:?}", self.range);
        }
        self.range = Some(range);
        self
    }

    /// Set the time range to be from `time` ago until now
    pub fn since(mut self, time: Duration) -> Self {
        if self.range.is_some() {
            panic!("already set range: {:?}", self.range);
        }
        let end = Utc::now();
        let start = end - TimeDelta::from_std(time).unwrap();

        self.range = Some(start..end);
        self
    }

    /// Set the step interval between data points
    pub fn step(mut self, step: Duration) -> Self {
        if self.step.is_some() {
            panic!("already set step: {:?}", self.step);
        }
        self.step = Some(step);
        self
    }

    /// Set the target number of data points (used to compute step if not set)
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Build the QueryRangeRequest
    pub fn build(self) -> QueryRangeRequest {
        let query = self.query;
        let range = self.range.unwrap();

        let step = self.step.unwrap_or_else(|| {
            let delta = range.end - range.start;
            (delta / (self.count as i32)).to_std().unwrap()
        });

        QueryRangeRequest {
            query,
            start: range.start,
            end: range.end,
            step: step.as_secs_f64(),
        }
    }
}
