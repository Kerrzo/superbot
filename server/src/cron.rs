//! Routines scheduler. Standard 5-field cron: minute hour day-of-month month day-of-week.
//! Supports *, numbers, lists (1,2), ranges (1-5), and steps (*/15, 1-10/2).
//! Uses the server's local time (set TZ).

use crate::runner::App;
use chrono::{Datelike, Local, Timelike};
use std::sync::Arc;

const RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

pub fn parse(expr: &str) -> Result<[Vec<bool>; 5], String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err("Cron needs 5 fields: minute hour day month weekday".into());
    }
    let mut out: [Vec<bool>; 5] = Default::default();
    for (i, f) in fields.iter().enumerate() {
        let (lo, hi) = RANGES[i];
        let mut set = vec![false; hi as usize + 1];
        for part in f.split(',') {
            let bad = || format!("Bad cron field \"{f}\"");
            let (range, step) = match part.split_once('/') {
                Some((r, s)) => (r, s.parse::<u32>().map_err(|_| bad())?),
                None => (part, 1),
            };
            let (start, end) = if range == "*" {
                (lo, hi)
            } else if let Some((a, b)) = range.split_once('-') {
                (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?)
            } else {
                let v: u32 = range.parse().map_err(|_| bad())?;
                (v, if part.contains('/') { hi } else { v })
            };
            if start < lo || end > hi || start > end || step == 0 {
                return Err(format!("Cron value out of range in \"{f}\""));
            }
            for v in (start..=end).step_by(step as usize) {
                set[if i == 4 && v == 7 { 0 } else { v as usize }] = true;
            }
        }
        out[i] = set;
    }
    Ok(out)
}

pub fn matches(expr: &str, t: &chrono::DateTime<Local>) -> bool {
    let Ok([min, hour, dom, mon, dow]) = parse(expr) else { return false };
    let f: Vec<&str> = expr.split_whitespace().collect();
    let (d, w) = (dom[t.day() as usize], dow[t.weekday().num_days_from_sunday() as usize]);
    // Classic cron: if both day fields are restricted, either one may match.
    let day_ok = if f[2] == "*" || f[4] == "*" { d && w } else { d || w };
    min[t.minute() as usize] && hour[t.hour() as usize] && mon[t.month() as usize] && day_ok
}

/// Checks every 15 seconds; each routine fires at most once per matching minute.
pub fn start(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tick.tick().await;
            let now = Local::now();
            let minute = now.timestamp() / 60;
            for r in app.db.routines(None) {
                if !r.enabled || r.last_run_at.is_some_and(|t| t / 60_000 == minute) || !matches(&r.cron, &now) {
                    continue;
                }
                app.dispatch_routine(&r, now.timestamp_millis());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cron() {
        let t = Local.with_ymd_and_hms(2026, 11, 16, 7, 30, 0).unwrap(); // a Monday
        assert!(matches("30 7 * * *", &t));
        assert!(matches("*/15 * * * *", &t));
        assert!(matches("30 7 * * 1-5", &t));
        assert!(!matches("30 7 * * 0,6", &t));
        assert!(matches("30 7 1 * 1", &t)); // day-of-month OR weekday
        assert!(!matches("0 8 * * *", &t));
        assert!(parse("61 * * * *").is_err());
        assert!(parse("* * *").is_err());
    }
}
