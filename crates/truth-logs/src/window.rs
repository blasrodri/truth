//! Time-window parsing and clamping to the configured maximum (spec §15.5).

/// Parse a window string like `7d`, `24h`, `30m` into seconds. Defaults to 7d
/// when unparseable.
pub fn window_secs(window: Option<&str>) -> i64 {
    let w = window.unwrap_or("7d").trim();
    let (num, unit) = w.split_at(w.len().saturating_sub(1));
    let n: i64 = num.parse().unwrap_or(7);
    match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        "w" => n * 604_800,
        _ => 7 * 86_400,
    }
}

/// Clamp a window (in seconds) to `max_days`.
pub fn clamp_window_secs(secs: i64, max_days: u32) -> i64 {
    let max = max_days as i64 * 86_400;
    secs.min(max).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(window_secs(Some("7d")), 7 * 86_400);
        assert_eq!(window_secs(Some("24h")), 24 * 3600);
        assert_eq!(window_secs(Some("30m")), 30 * 60);
    }

    #[test]
    fn clamps() {
        assert_eq!(clamp_window_secs(90 * 86_400, 30), 30 * 86_400);
        assert_eq!(clamp_window_secs(3 * 86_400, 30), 3 * 86_400);
    }
}
