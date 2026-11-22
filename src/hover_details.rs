//! Text for the widget's hover tooltip: per usage window, the used and
//! remaining share, reset time, pace, and (where local transcripts exist)
//! tokens and estimated cost for the window so far.

use std::time::{Duration, SystemTime};

use crate::models::{AppUsageData, UsageData, UsageSection};
use crate::usage_stats::{LocalTokenStats, TokenTotals, WindowTotals, SESSION_WINDOW, WEEKLY_WINDOW};

/// Pace differences smaller than this many points read as "on pace".
const ON_PACE_POINTS: f64 = 2.0;

struct Window<'a> {
    label: &'static str,
    section: &'a UsageSection,
    duration: Option<Duration>,
    tokens: Option<TokenTotals>,
}

/// Tooltip text for widget column `index` (the widget's provider order:
/// Claude Code, Codex, Antigravity Gemini, Antigravity Claude/GPT,
/// Antigravity Argon, Grok, Cursor). `clock` formats a reset time in local time.
pub fn provider_details(
    index: usize,
    data: Option<&AppUsageData>,
    stats: &LocalTokenStats,
    now: SystemTime,
    clock: &dyn Fn(SystemTime) -> String,
) -> String {
    let (title, usage, tokens, priced, top, bottom_label, bottom_duration) = match index {
        0 => ("Claude Code", data.and_then(|d| d.claude_code.as_ref()), stats.claude, true, true, "7-day", Some(WEEKLY_WINDOW)),
        1 => ("Codex", data.and_then(|d| d.codex.as_ref()), stats.codex, false, true, "7-day", Some(WEEKLY_WINDOW)),
        2 => ("Antigravity: Gemini", data.and_then(|d| d.antigravity.as_ref()), WindowTotals::default(), false, true, "7-day", Some(WEEKLY_WINDOW)),
        3 => ("Antigravity: Claude/GPT", data.and_then(|d| d.antigravity_claude.as_ref()), WindowTotals::default(), false, true, "7-day", Some(WEEKLY_WINDOW)),
        4 => ("Antigravity: Argon", data.and_then(|d| d.antigravity_argon.as_ref()), WindowTotals::default(), false, false, "7-day", Some(WEEKLY_WINDOW)),
        5 => ("Grok", data.and_then(|d| d.grok.as_ref()), WindowTotals::default(), false, false, "7-day", Some(WEEKLY_WINDOW)),
        6 => ("Cursor", data.and_then(|d| d.cursor.as_ref()), WindowTotals::default(), false, false, "Monthly", None),
        _ => return String::new(),
    };
    let Some(usage) = usage else {
        return format!("{title}\nNo usage data yet");
    };
    let windows = windows_for(usage, tokens, top, bottom_label, bottom_duration);
    let mut lines = vec![title.to_string()];
    for window in windows {
        lines.extend(window_lines(&window, priced, now, clock));
    }
    if priced && lines.iter().any(|line| line.contains("Cost")) {
        lines.push("Cost is an estimate at API list prices.".to_string());
    }
    lines.join("\r\n")
}

fn windows_for<'a>(
    usage: &'a UsageData,
    tokens: WindowTotals,
    top: bool,
    bottom_label: &'static str,
    bottom_duration: Option<Duration>,
) -> Vec<Window<'a>> {
    let mut windows = Vec::new();
    if top {
        windows.push(Window {
            label: "5-hour",
            section: &usage.session,
            duration: Some(SESSION_WINDOW),
            tokens: tokens.session,
        });
    }
    windows.push(Window {
        label: bottom_label,
        section: &usage.weekly,
        duration: bottom_duration,
        tokens: tokens.weekly,
    });
    windows
}

fn window_lines(
    window: &Window,
    priced: bool,
    now: SystemTime,
    clock: &dyn Fn(SystemTime) -> String,
) -> Vec<String> {
    let used = window.section.percentage;
    if !used.is_finite() {
        return vec![format!("{}: no data", window.label)];
    }
    let used = used.clamp(0.0, 100.0);
    // A window with no reset time and nothing used has not started (or the
    // plan has no such window); "100% left" would read as a real balance.
    if used == 0.0 && window.section.resets_at.is_none() {
        return vec![format!("{}: no active window", window.label)];
    }
    let mut lines = vec![format!(
        "{}: {:.0}% used, {:.0}% left{}",
        window.label,
        used,
        100.0 - used,
        window
            .duration
            .map(|d| format!(" ({} window)", duration_text(d)))
            .unwrap_or_default()
    )];
    if let Some(resets_at) = window.section.resets_at {
        let reset = match resets_at.duration_since(now) {
            Ok(left) => format!("  Resets in {} ({})", duration_text(left), clock(resets_at)),
            Err(_) => format!("  Reset due ({})", clock(resets_at)),
        };
        lines.push(reset);
        if let Some(pace) = window.duration.and_then(|d| pace_text(used, resets_at, d, now)) {
            lines.push(format!("  Pace: {pace}"));
        }
    }
    if let Some(tokens) = window.tokens {
        lines.push(format!(
            "  Tokens: {} in, {} out, {} cache read, {} cache write",
            compact(tokens.input),
            compact(tokens.output),
            compact(tokens.cache_read),
            compact(tokens.cache_write)
        ));
        if priced && tokens.total() > 0 {
            lines.push(format!(
                "  Cost: ${:.2} est.{}",
                tokens.cost_usd,
                if tokens.cost_incomplete { " (some models unpriced)" } else { "" }
            ));
        }
    }
    lines
}

/// Share of the window that has elapsed, in percent.
fn elapsed_percent(resets_at: SystemTime, duration: Duration, now: SystemTime) -> Option<f64> {
    let start = resets_at.checked_sub(duration)?;
    let elapsed = now.duration_since(start).unwrap_or_default();
    Some((elapsed.as_secs_f64() / duration.as_secs_f64() * 100.0).clamp(0.0, 100.0))
}

/// "N pts ahead/behind" compares the share used with the share of time elapsed.
fn pace_text(used: f64, resets_at: SystemTime, duration: Duration, now: SystemTime) -> Option<String> {
    let elapsed = elapsed_percent(resets_at, duration, now)?;
    let delta = used - elapsed;
    let verdict = if delta.abs() < ON_PACE_POINTS {
        "on pace".to_string()
    } else if delta > 0.0 {
        format!("{:.0} pts ahead (using faster than time passes)", delta)
    } else {
        format!("{:.0} pts behind (room to spare)", -delta)
    };
    Some(format!("{verdict}; {:.0}% of window elapsed", elapsed))
}

fn duration_text(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    let (days, hours, mins) = (minutes / 1440, (minutes / 60) % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) if mins == 0 => format!("{hours}h"),
        (0, _) => format!("{hours}h {mins}m"),
        (_, 0) => format!("{days}d"),
        _ => format!("{days}d {hours}h"),
    }
}

fn compact(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}K", value / 1e3)
    } else if tokens < 1_000_000_000 {
        format!("{:.1}M", value / 1e6)
    } else {
        format!("{:.2}B", value / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn section(percentage: f64, resets_at: Option<SystemTime>) -> UsageSection {
        UsageSection { percentage, resets_at }
    }

    #[test]
    fn hover_pace_compares_used_share_with_elapsed_share() {
        let now = at(1_000_000);
        // 5-hour window resetting in 3.5 h: 1.5 h (30%) elapsed.
        let resets = now + Duration::from_secs(12_600);
        assert_eq!(elapsed_percent(resets, SESSION_WINDOW, now).unwrap().round(), 30.0);
        assert!(pace_text(42.0, resets, SESSION_WINDOW, now).unwrap().starts_with("12 pts ahead"));
        assert!(pace_text(10.0, resets, SESSION_WINDOW, now).unwrap().starts_with("20 pts behind"));
        assert!(pace_text(31.0, resets, SESSION_WINDOW, now).unwrap().starts_with("on pace"));
    }

    #[test]
    fn hover_claude_details_show_resets_pace_tokens_and_cost() {
        let now = at(1_000_000);
        let data = AppUsageData {
            claude_code: Some(UsageData {
                session: section(42.0, Some(now + Duration::from_secs(12_600))),
                weekly: section(63.0, Some(now + Duration::from_secs(2 * 86_400 + 3_600))),
            }),
            ..Default::default()
        };
        let tokens = TokenTotals {
            input: 1_234,
            output: 45_600,
            cache_read: 30_000_000,
            cache_write: 2_000_000,
            cost_usd: 12.345,
            cost_incomplete: false,
        };
        let stats = LocalTokenStats {
            claude: WindowTotals {
                session: Some(tokens),
                weekly: Some(tokens),
            },
            ..Default::default()
        };
        let text = provider_details(0, Some(&data), &stats, now, &|_| "Sat 14:30".into());

        assert!(text.starts_with("Claude Code\r\n"));
        assert!(text.contains("5-hour: 42% used, 58% left (5h window)"));
        assert!(text.contains("Resets in 3h 30m (Sat 14:30)"));
        assert!(text.contains("Pace: 12 pts ahead"));
        assert!(text.contains("7-day: 63% used, 37% left (7d window)"));
        assert!(text.contains("Resets in 2d 1h"));
        assert!(text.contains("Tokens: 1.2K in, 45.6K out, 30.0M cache read, 2.0M cache write"));
        assert!(text.contains("Cost: $12.35 est."));
    }

    #[test]
    fn hover_handles_missing_data_weekly_only_and_unknown_duration() {
        let now = at(1_000_000);
        let stats = LocalTokenStats::default();
        assert_eq!(
            provider_details(5, None, &stats, now, &|_| String::new()),
            "Grok\nNo usage data yet"
        );
        let data = AppUsageData {
            grok: Some(UsageData {
                session: section(0.0, None),
                weekly: section(20.0, Some(now + Duration::from_secs(3_600))),
            }),
            cursor: Some(UsageData {
                session: section(f64::NAN, None),
                weekly: section(50.0, Some(now + Duration::from_secs(3_600))),
            }),
            codex: Some(UsageData {
                session: section(f64::NAN, None),
                weekly: section(10.0, None),
            }),
            ..Default::default()
        };
        let grok = provider_details(5, Some(&data), &stats, now, &|_| "x".into());
        assert!(!grok.contains("5-hour"));
        assert!(grok.contains("7-day: 20% used"));
        let cursor = provider_details(6, Some(&data), &stats, now, &|_| "x".into());
        assert!(cursor.contains("Monthly: 50% used, 50% left\r\n"));
        assert!(!cursor.contains("Pace"));
        let codex = provider_details(1, Some(&data), &stats, now, &|_| "x".into());
        assert!(codex.contains("5-hour: no data"));
        let idle = AppUsageData {
            claude_code: Some(UsageData {
                session: section(0.0, None),
                weekly: section(7.0, Some(now + Duration::from_secs(3_600))),
            }),
            ..Default::default()
        };
        let claude = provider_details(0, Some(&idle), &stats, now, &|_| "x".into());
        assert!(claude.contains("5-hour: no active window"));
        assert!(!claude.contains("100% left"));
        assert!(!codex.contains("Cost"));
    }
}
