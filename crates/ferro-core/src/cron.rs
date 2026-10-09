//! Five-field cron expressions (`minute hour day-of-month month day-of-week`),
//! evaluated in UTC. Supports `*`, lists, ranges and steps (`*/15`, `1-5`, `10-50/10`).
//! Day-of-month and day-of-week follow the classic rule: when both are restricted,
//! either one matching is enough.

use chrono::{DateTime, Datelike, Timelike, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
}

fn field(spec: &str, min: u32, max: u32) -> Result<(u64, bool), String> {
    let num = |s: &str| -> Result<u32, String> {
        s.parse::<u32>().map_err(|_| format!("invalid number `{s}` in cron field `{spec}`"))
    };
    let mut mask = 0u64;
    for part in spec.split(',') {
        if part.is_empty() {
            return Err(format!("empty element in cron field `{spec}`"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, num(s)?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step 0 in cron field `{spec}`"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a)?, num(b)?)
        } else {
            let n = num(range)?;
            if part.contains('/') {
                (n, max)
            } else {
                (n, n)
            }
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("cron field `{spec}` must be within {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1u64 << v;
            v = v.saturating_add(step);
        }
    }
    Ok((mask, spec == "*"))
}

impl Cron {
    pub fn parse(spec: &str) -> Result<Cron, String> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!("a cron expression has 5 fields (minute hour day month weekday), got {}", parts.len()));
        }
        let (minute, _) = field(parts[0], 0, 59)?;
        let (hour, _) = field(parts[1], 0, 23)?;
        let (dom, dom_any) = field(parts[2], 1, 31)?;
        let (month, _) = field(parts[3], 1, 12)?;
        let (mut dow, dow_any) = field(parts[4], 0, 7)?;
        if dow & (1 << 7) != 0 {
            // 7 is Sunday as well
            dow |= 1;
            dow &= !(1 << 7);
        }
        Ok(Cron { minute, hour, dom, month, dow, dom_any, dow_any })
    }

    pub fn matches(&self, t: &DateTime<Utc>) -> bool {
        let bit = |mask: u64, n: u32| (mask >> n) & 1 == 1;
        if !(bit(self.minute, t.minute()) && bit(self.hour, t.hour()) && bit(self.month, t.month())) {
            return false;
        }
        let dom_ok = bit(self.dom, t.day());
        let dow_ok = bit(self.dow, t.weekday().num_days_from_sunday());
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn steps_lists_and_ranges() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert!(c.matches(&at(2026, 10, 5, 10, 30)));
        assert!(c.matches(&at(2026, 10, 5, 10, 0)));
        assert!(!c.matches(&at(2026, 10, 5, 10, 31)));
        let c = Cron::parse("5,10-12 8 * * *").unwrap();
        assert!(c.matches(&at(2026, 10, 5, 8, 11)));
        assert!(!c.matches(&at(2026, 10, 5, 9, 11)));
        assert!(Cron::parse("10-50/20 * * * *").unwrap().matches(&at(2026, 10, 5, 0, 30)));
    }

    #[test]
    fn weekdays_and_day_rules() {
        // 2026-10-05 is a Monday, 2026-10-10 a Saturday, 2026-10-11 a Sunday
        let weekdays = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(weekdays.matches(&at(2026, 10, 5, 9, 0)));
        assert!(!weekdays.matches(&at(2026, 10, 10, 9, 0)));
        let sunday7 = Cron::parse("0 0 * * 7").unwrap();
        assert!(sunday7.matches(&at(2026, 10, 11, 0, 0)));
        // day-of-month OR weekday when both are restricted
        let either = Cron::parse("0 0 1 * 1").unwrap();
        assert!(either.matches(&at(2026, 10, 1, 0, 0)), "the 1st");
        assert!(either.matches(&at(2026, 10, 5, 0, 0)), "a Monday");
        assert!(!either.matches(&at(2026, 10, 6, 0, 0)));
    }

    #[test]
    fn rejects_invalid_expressions() {
        for bad in ["* * *", "61 * * * *", "* 24 * * *", "* * 0 * *", "*/0 * * * *", "a * * * *", "5-2 * * * *", "1,,2 * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }
}
