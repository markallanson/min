//! min plugin: a `clock` tool that reports the current UTC time.
//!
//! A crate, a struct, two impls — exactly the README's "Writing a plugin"
//! example, made real. No dependencies beyond `min-core`: the calendar math
//! (days since epoch → Y-M-D) is done by hand.

use min_core::{Hub, Plugin, Tool};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn name(&self) -> &'static str {
        "clock"
    }
    fn start(&mut self, hub: &Hub) {
        hub.tools().register(Box::new(ClockTool));
    }
}

struct ClockTool;

impl Tool for ClockTool {
    fn name(&self) -> &'static str {
        "clock"
    }
    fn spec(&self) -> &'static str {
        r#"{"description":"Current UTC time","input_schema":{"type":"object","properties":{}}}"#
    }
    fn run(&self, _input: &str) -> Result<String, String> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        Ok(format_utc(secs))
    }
}

/// Format seconds-since-epoch as `YYYY-MM-DD HH:MM:SS` (UTC).
pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let rem = secs % 86_400;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn known_moments() {
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00"); // leap day
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(format_utc(86_399), "1970-01-01 23:59:59");
        assert_eq!(format_utc(2_147_483_648), "2038-01-19 03:14:08"); // y2038 boundary
    }
}
