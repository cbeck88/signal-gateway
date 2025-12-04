//! Display wrapper for formatting chrono::TimeDelta as "T-duration"

use chrono::TimeDelta;
use std::{
    fmt::{self, Display},
    time::Duration,
};

/// A display wrapper that formats a chrono::TimeDelta as "T-duration" or "T+duration"
/// with coarse precision (truncated to minutes for durations > 1 hour).
#[derive(Clone, Copy, Debug)]
pub struct HumanTMinus(pub TimeDelta);

impl Display for HumanTMinus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prefix, abs_duration) = if self.0.num_seconds() >= 0 {
            ("T-", self.0)
        } else {
            ("T+", -self.0)
        };

        let std_dur = abs_duration.to_std().unwrap_or_default();
        let mut secs = std_dur.as_secs();

        // Reduce precision to minutes for durations > 1 hour
        if secs > 3600 {
            secs -= secs % 60;
        }

        let coarse_dur = Duration::new(secs, 0);
        // Remove spaces for compact format (e.g., "T-1m30s" not "T-1m 30s")
        let formatted = humantime::format_duration(coarse_dur)
            .to_string()
            .replace(' ', "");
        write!(f, "{}{}", prefix, formatted)
    }
}

impl From<TimeDelta> for HumanTMinus {
    fn from(td: TimeDelta) -> Self {
        HumanTMinus(td)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_t_minus_positive() {
        assert_eq!(HumanTMinus(TimeDelta::seconds(30)).to_string(), "T-30s");
        assert_eq!(HumanTMinus(TimeDelta::seconds(90)).to_string(), "T-1m30s");
        assert_eq!(HumanTMinus(TimeDelta::hours(1)).to_string(), "T-1h");
        // > 1 hour: truncate to minutes
        assert_eq!(HumanTMinus(TimeDelta::seconds(3700)).to_string(), "T-1h1m");
    }

    #[test]
    fn test_human_t_minus_negative() {
        assert_eq!(HumanTMinus(TimeDelta::seconds(-30)).to_string(), "T+30s");
        assert_eq!(HumanTMinus(TimeDelta::seconds(-3700)).to_string(), "T+1h1m");
    }
}
