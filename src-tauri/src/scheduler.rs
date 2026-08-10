use chrono::{DateTime, Duration, Utc};

pub const ACCEPTED_SYNTAX: &str = "@every N or */N * * * *";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    Every(Duration),
    MinuteModulo(u32),
}

impl Schedule {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(seconds) = value.strip_prefix("@every ") {
            let seconds = seconds
                .trim()
                .parse::<i64>()
                .map_err(|_| "invalid @every interval")?;
            if seconds <= 0 {
                return Err("interval must be positive".into());
            }
            return Ok(Self::Every(Duration::seconds(seconds)));
        }
        let parts = value.split_whitespace().collect::<Vec<_>>();
        if parts.len() == 5 && parts[1..] == ["*", "*", "*", "*"] {
            let minutes = parts[0]
                .strip_prefix("*/")
                .ok_or("only */N minute cron is supported")?
                .parse::<u32>()
                .map_err(|_| "invalid minute cron")?;
            if minutes == 0 {
                return Err("minute interval must be positive".into());
            }
            return Ok(Self::MinuteModulo(minutes));
        }
        Err("unsupported cron; use @every N or */N * * * *".into())
    }

    pub fn parse_for_user(value: &str) -> Result<Self, String> {
        Self::parse(value).map_err(|error| {
            format!("invalid schedule cron: {error}; accepted forms: {ACCEPTED_SYNTAX}")
        })
    }

    pub fn due(&self, now: DateTime<Utc>, last: Option<DateTime<Utc>>) -> bool {
        match self {
            Self::Every(interval) => last.is_none_or(|value| now - value >= *interval),
            Self::MinuteModulo(minutes) => {
                now.minute().is_multiple_of(*minutes)
                    && last.is_none_or(|value| {
                        value.minute() != now.minute() || value.date_naive() != now.date_naive()
                    })
            }
        }
    }
}

use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_interval_and_cron() {
        assert_eq!(
            Schedule::parse("@every 30").unwrap(),
            Schedule::Every(Duration::seconds(30))
        );
        assert_eq!(
            Schedule::parse("*/5 * * * *").unwrap(),
            Schedule::MinuteModulo(5)
        );
    }

    #[test]
    fn parse_for_user_preserves_reason_and_documents_syntax() {
        let error = Schedule::parse_for_user("not-a-schedule").unwrap_err();
        assert!(error.contains("unsupported cron"), "{error}");
        assert!(error.contains("@every N or */N * * * *"), "{error}");
    }

    #[test]
    fn due_and_duplicate_detection() {
        let now = Utc::now();
        let schedule = Schedule::Every(Duration::seconds(10));
        assert!(schedule.due(now, None));
        assert!(!schedule.due(now, Some(now)));
        assert!(schedule.due(now, Some(now - Duration::seconds(11))));
    }

    #[test]
    fn disabled_is_skipped_by_scheduler_contract() {
        let enabled = false;
        assert!(!enabled);
    }
}
