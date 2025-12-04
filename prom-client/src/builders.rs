use super::QueryRangeRequest;
use chrono::{DateTime, TimeDelta, Utc};
use std::{ops::Range, time::Duration};

pub struct QueryRangeRequestBuilder {
    query: String,
    range: Option<Range<DateTime<Utc>>>,
    step: Option<Duration>,
    count: usize,
}

impl QueryRangeRequestBuilder {
    pub fn new(query: String) -> Self {
        Self {
            query,
            range: None,
            step: None,
            count: 256,
        }
    }

    pub fn range(mut self, range: Range<DateTime<Utc>>) -> Self {
        if self.range.is_some() {
            panic!("already set range: {:?}", self.range);
        }
        self.range = Some(range);
        self
    }

    pub fn since(mut self, time: Duration) -> Self {
        if self.range.is_some() {
            panic!("already set range: {:?}", self.range);
        }
        let end = Utc::now();
        let start = end - TimeDelta::from_std(time).unwrap();

        self.range = Some(start..end);
        self
    }

    pub fn step(mut self, step: Duration) -> Self {
        if self.step.is_some() {
            panic!("already set step: {:?}", self.step);
        }
        self.step = Some(step);
        self
    }

    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

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
