//! Token totals per usage window, read from local transcripts.
//!
//! Claude Code: `~/.claude/projects/**/*.jsonl` assistant records
//! (`message.usage`, `message.model`). Streaming writes the same message more
//! than once, so records are deduplicated by message id + request id.
//! Codex: `~/.codex/sessions/**/*.jsonl` `token_count` events, using the change
//! in each file's cumulative `total_token_usage`.
//!
//! Files are read incrementally (byte offset per file, only complete lines),
//! and files untouched for longer than the retention window are never opened,
//! so a one-minute poll does not re-read gigabytes of history.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::AppUsageData;
use crate::pricing::{self, ModelPrice};

/// Longest window shown (7 days) plus slack for a reset that is late.
const RETENTION: Duration = Duration::from_secs(8 * 24 * 3600);
pub const SESSION_WINDOW: Duration = Duration::from_secs(5 * 3600);
pub const WEEKLY_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
    /// True when some tokens came from a model with no known price.
    pub cost_incomplete: bool,
}

impl TokenTotals {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowTotals {
    pub session: Option<TokenTotals>,
    pub weekly: Option<TokenTotals>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LocalTokenStats {
    pub claude: WindowTotals,
    pub codex: WindowTotals,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct UsageRecord {
    at: i64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    price: Option<ModelPrice>,
}

impl UsageRecord {
    fn cost(&self) -> Option<f64> {
        let p = self.price?;
        let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        Some(
            per(self.input, p.input)
                + per(self.output, p.output)
                + per(self.cache_read, p.cache_read)
                + per(self.cache_write_5m, p.cache_write_5m())
                + per(self.cache_write_1h, p.cache_write_1h()),
        )
    }

    fn billable_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CodexCumulative {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

#[derive(Default)]
struct FileCursor {
    offset: u64,
    len: u64,
    modified: Option<SystemTime>,
    codex_previous: Option<CodexCumulative>,
}

struct TranscriptScanner {
    source: Source,
    files: HashMap<PathBuf, FileCursor>,
    /// Claude records keyed by message id + request id (later copies win).
    keyed: HashMap<String, UsageRecord>,
    unkeyed: Vec<UsageRecord>,
}

impl TranscriptScanner {
    fn new(source: Source) -> Self {
        Self {
            source,
            files: HashMap::new(),
            keyed: HashMap::new(),
            unkeyed: Vec::new(),
        }
    }

    fn scan(&mut self, root: &Path, now: SystemTime) {
        let cutoff = now.checked_sub(RETENTION).unwrap_or(UNIX_EPOCH);
        let mut paths = Vec::new();
        collect_jsonl(root, cutoff, &mut paths);
        for (path, len, modified) in paths {
            let cursor = self.files.entry(path.clone()).or_default();
            if cursor.len == len && cursor.modified == Some(modified) {
                continue;
            }
            if len < cursor.offset {
                *cursor = FileCursor::default();
            }
            let Some(bytes) = read_from(&path, cursor.offset) else {
                continue;
            };
            let consumed = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let mut previous = cursor.codex_previous;
            for line in bytes[..consumed].split(|b| *b == b'\n') {
                let Ok(line) = std::str::from_utf8(line) else {
                    continue;
                };
                match self.source {
                    Source::Claude => {
                        if let Some((key, record)) = parse_claude_line(line) {
                            match key {
                                Some(key) => {
                                    self.keyed.insert(key, record);
                                }
                                None => self.unkeyed.push(record),
                            }
                        }
                    }
                    Source::Codex => {
                        if let Some(record) = parse_codex_line(line, &mut previous) {
                            self.unkeyed.push(record);
                        }
                    }
                }
            }
            let cursor = self.files.get_mut(&path).expect("cursor inserted above");
            cursor.offset += consumed as u64;
            cursor.len = len;
            cursor.modified = Some(modified);
            cursor.codex_previous = previous;
        }

        let oldest = unix_secs(cutoff);
        self.keyed.retain(|_, record| record.at >= oldest);
        self.unkeyed.retain(|record| record.at >= oldest);
        self.files
            .retain(|_, cursor| cursor.modified.is_some_and(|modified| modified >= cutoff));
    }

    fn totals_between(&self, start: i64, end: i64) -> TokenTotals {
        let mut totals = TokenTotals::default();
        for record in self.keyed.values().chain(self.unkeyed.iter()) {
            if record.at < start || record.at > end {
                continue;
            }
            totals.input += record.input;
            totals.output += record.output;
            totals.cache_read += record.cache_read;
            totals.cache_write += record.cache_write_5m + record.cache_write_1h;
            match record.cost() {
                Some(cost) => totals.cost_usd += cost,
                None if record.billable_tokens() > 0 => totals.cost_incomplete = true,
                None => {}
            }
        }
        totals
    }
}

fn collect_jsonl(dir: &Path, cutoff: SystemTime, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl(&path, cutoff, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified >= cutoff {
                out.push((path, metadata.len(), modified));
            }
        }
    }
}

fn read_from(path: &Path, offset: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn unix_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn json_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn record_time(value: &serde_json::Value) -> Option<i64> {
    let text = value.get("timestamp")?.as_str()?;
    crate::poller::parse_iso8601(Some(text)).map(unix_secs)
}

/// Returns the dedupe key (message id + request id, when present) and record.
fn parse_claude_line(line: &str) -> Option<(Option<String>, UsageRecord)> {
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let at = record_time(&value)?;
    let model = message
        .get("model")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("requestedModel").and_then(serde_json::Value::as_str))
        .unwrap_or("");

    let cache_write_total = json_u64(usage, "cache_creation_input_tokens");
    let (cache_write_5m, cache_write_1h) = match usage.get("cache_creation") {
        Some(split) => {
            let one_hour = json_u64(split, "ephemeral_1h_input_tokens");
            let five_min = json_u64(split, "ephemeral_5m_input_tokens");
            // Anything the split does not account for is billed at the 5-minute rate.
            let rest = cache_write_total.saturating_sub(one_hour + five_min);
            (five_min + rest, one_hour)
        }
        None => (cache_write_total, 0),
    };
    let record = UsageRecord {
        at,
        input: json_u64(usage, "input_tokens"),
        output: json_u64(usage, "output_tokens"),
        cache_read: json_u64(usage, "cache_read_input_tokens"),
        cache_write_5m,
        cache_write_1h,
        price: pricing::claude_price(model),
    };

    let message_id = message.get("id").and_then(serde_json::Value::as_str);
    let request_id = value.get("requestId").and_then(serde_json::Value::as_str);
    let key = match (message_id, request_id) {
        (None, None) => None,
        (m, r) => Some(format!("{}:{}", m.unwrap_or(""), r.unwrap_or(""))),
    };
    Some((key, record))
}

fn parse_codex_line(line: &str, previous: &mut Option<CodexCumulative>) -> Option<UsageRecord> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let info = payload.get("info")?;
    let cumulative = |usage: &serde_json::Value| CodexCumulative {
        input: json_u64(usage, "input_tokens"),
        cached: json_u64(usage, "cached_input_tokens"),
        cache_write: json_u64(usage, "cache_write_input_tokens"),
        output: json_u64(usage, "output_tokens"),
    };
    let total = cumulative(info.get("total_token_usage")?);
    let delta = match *previous {
        Some(prev) if total == prev => {
            return None;
        }
        Some(prev)
            if total.input >= prev.input
                && total.cached >= prev.cached
                && total.cache_write >= prev.cache_write
                && total.output >= prev.output =>
        {
            CodexCumulative {
                input: total.input - prev.input,
                cached: total.cached - prev.cached,
                cache_write: total.cache_write - prev.cache_write,
                output: total.output - prev.output,
            }
        }
        // First event in the file, or the counter restarted.
        _ => info.get("last_token_usage").map(cumulative).unwrap_or(total),
    };
    *previous = Some(total);
    Some(UsageRecord {
        at: record_time(&value)?,
        // OpenAI counts cached input inside input_tokens.
        input: delta.input.saturating_sub(delta.cached),
        output: delta.output,
        cache_read: delta.cached,
        cache_write_5m: delta.cache_write,
        cache_write_1h: 0,
        price: None,
    })
}

/// Window bounds `[resets_at - duration, now]`, or None without a reset time.
fn window_bounds(
    resets_at: Option<SystemTime>,
    duration: Duration,
    now: SystemTime,
) -> Option<(i64, i64)> {
    let start = resets_at?.checked_sub(duration)?;
    Some((unix_secs(start), unix_secs(now)))
}

fn window_totals(
    scanner: &TranscriptScanner,
    usage: Option<&crate::models::UsageData>,
    now: SystemTime,
) -> WindowTotals {
    let Some(usage) = usage else {
        return WindowTotals::default();
    };
    let totals = |resets_at, duration| {
        window_bounds(resets_at, duration, now).map(|(start, end)| scanner.totals_between(start, end))
    };
    WindowTotals {
        session: totals(usage.session.resets_at, SESSION_WINDOW),
        weekly: totals(usage.weekly.resets_at, WEEKLY_WINDOW),
    }
}

static CLAUDE_SCANNER: Mutex<Option<TranscriptScanner>> = Mutex::new(None);
static CODEX_SCANNER: Mutex<Option<TranscriptScanner>> = Mutex::new(None);
static LATEST: Mutex<LocalTokenStats> = Mutex::new(LocalTokenStats {
    claude: WindowTotals {
        session: None,
        weekly: None,
    },
    codex: WindowTotals {
        session: None,
        weekly: None,
    },
});

fn refresh_source(
    slot: &Mutex<Option<TranscriptScanner>>,
    source: Source,
    root: Option<PathBuf>,
    usage: Option<&crate::models::UsageData>,
    now: SystemTime,
) -> WindowTotals {
    let (Some(root), Some(_)) = (root, usage) else {
        return WindowTotals::default();
    };
    let Ok(mut guard) = slot.lock() else {
        return WindowTotals::default();
    };
    let scanner = guard.get_or_insert_with(|| TranscriptScanner::new(source));
    scanner.scan(&root, now);
    window_totals(scanner, usage, now)
}

/// Rescan transcripts for the providers that have usage data and store the
/// totals for the hover. Call from a background thread (the poll thread).
pub fn refresh(data: &AppUsageData) {
    let now = SystemTime::now();
    let home = dirs::home_dir();
    let claude = refresh_source(
        &CLAUDE_SCANNER,
        Source::Claude,
        home.as_ref().map(|h| h.join(".claude").join("projects")),
        data.claude_code.as_ref(),
        now,
    );
    let codex = refresh_source(
        &CODEX_SCANNER,
        Source::Codex,
        home.as_ref().map(|h| h.join(".codex").join("sessions")),
        data.codex.as_ref(),
        now,
    );
    if let Ok(mut latest) = LATEST.lock() {
        *latest = LocalTokenStats { claude, codex };
    }
}

pub fn latest() -> LocalTokenStats {
    LATEST.lock().map(|stats| *stats).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(ts: &str, id: &str, req: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{req}","message":{{"id":"{id}","model":"claude-opus-5-5","usage":{{"input_tokens":10,"output_tokens":{output},"cache_read_input_tokens":1000,"cache_creation_input_tokens":300,"cache_creation":{{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200}}}}}}}}"#
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccum-stats-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project").join("subagents")).unwrap();
        dir
    }

    #[test]
    fn stats_claude_dedupes_streamed_copies_and_prices_cache_tiers() {
        let dir = temp_dir("claude");
        let path = dir.join("project").join("a.jsonl");
        let lines = [
            claude_line("2026-11-22T06:00:00Z", "msg_1", "req_1", 5),
            claude_line("2026-11-22T06:00:01Z", "msg_1", "req_1", 50),
            claude_line("2026-11-22T06:10:00Z", "msg_2", "req_2", 50),
            r#"{"type":"user","message":{"content":"usage assistant"}}"#.to_string(),
            "not json".to_string(),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        // A subagent transcript in a nested folder is found too.
        std::fs::write(
            dir.join("project").join("subagents").join("b.jsonl"),
            claude_line("2026-11-22T06:20:00Z", "msg_3", "req_3", 50) + "\n",
        )
        .unwrap();

        let now = crate::poller::parse_iso8601(Some("2026-11-22T07:00:00Z")).unwrap();
        let mut scanner = TranscriptScanner::new(Source::Claude);
        scanner.scan(&dir, now);
        let start = unix_secs(now) - 3600 * 2;
        let totals = scanner.totals_between(start, unix_secs(now));

        assert_eq!(totals.input, 30);
        assert_eq!(totals.output, 150);
        assert_eq!(totals.cache_read, 3000);
        assert_eq!(totals.cache_write, 900);
        // Per message: 10*4 + 50*20 + 1000*0.2 + 100*5 + 200*8 = 3340 micro-dollars.
        assert!((totals.cost_usd - 3.0 * 0.003340).abs() < 1e-9, "{}", totals.cost_usd);
        assert!(!totals.cost_incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_scan_is_incremental_and_waits_for_complete_lines() {
        let dir = temp_dir("incremental");
        let path = dir.join("project").join("a.jsonl");
        let now = crate::poller::parse_iso8601(Some("2026-11-22T07:00:00Z")).unwrap();
        let first = claude_line("2026-11-22T06:00:00Z", "msg_1", "req_1", 50);
        let second = claude_line("2026-11-22T06:30:00Z", "msg_2", "req_2", 50);
        // The second record is still being written: no trailing newline.
        std::fs::write(&path, format!("{first}\n{}", &second[..40])).unwrap();

        let mut scanner = TranscriptScanner::new(Source::Claude);
        scanner.scan(&dir, now);
        let window = (unix_secs(now) - 7200, unix_secs(now));
        assert_eq!(scanner.totals_between(window.0, window.1).output, 50);
        let offset = scanner.files.values().next().unwrap().offset;
        assert_eq!(offset as usize, first.len() + 1);

        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        scanner.scan(&dir, now);
        assert_eq!(scanner.totals_between(window.0, window.1).output, 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_window_excludes_records_before_window_start() {
        let dir = temp_dir("window");
        std::fs::write(
            dir.join("project").join("a.jsonl"),
            [
                claude_line("2026-11-22T01:00:00Z", "msg_old", "r0", 999),
                claude_line("2026-11-22T06:00:00Z", "msg_new", "r1", 7),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let now = crate::poller::parse_iso8601(Some("2026-11-22T07:00:00Z")).unwrap();
        let resets = crate::poller::parse_iso8601(Some("2026-11-22T09:00:00Z"));
        let (start, end) = window_bounds(resets, SESSION_WINDOW, now).unwrap();
        let mut scanner = TranscriptScanner::new(Source::Claude);
        scanner.scan(&dir, now);
        assert_eq!(scanner.totals_between(start, end).output, 7);
        assert!(window_bounds(None, SESSION_WINDOW, now).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "Reads the local transcript folders to time a first and a repeat scan"]
    fn stats_live_scan_timing() {
        let home = dirs::home_dir().unwrap();
        for (source, root) in [
            (Source::Claude, home.join(".claude").join("projects")),
            (Source::Codex, home.join(".codex").join("sessions")),
        ] {
            let now = SystemTime::now();
            let mut scanner = TranscriptScanner::new(source);
            let started = std::time::Instant::now();
            scanner.scan(&root, now);
            let first = started.elapsed();
            let started = std::time::Instant::now();
            scanner.scan(&root, SystemTime::now());
            let repeat = started.elapsed();
            let end = unix_secs(now);
            let five_h = scanner.totals_between(end - 5 * 3600, end);
            let week = scanner.totals_between(end - 7 * 86_400, end);
            println!(
                "{source:?}: files={} records={} first={first:?} repeat={repeat:?}",
                scanner.files.len(),
                scanner.keyed.len() + scanner.unkeyed.len()
            );
            println!("  last 5h: {five_h:?}");
            println!("  last 7d: {week:?}");
        }
    }

    #[test]
    fn stats_codex_uses_cumulative_deltas_and_skips_repeats() {
        let event = |ts: &str, input: u64, cached: u64, output: u64| {
            format!(
                r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output}}}}}}}}}"#
            )
        };
        let mut previous = None;
        let first = parse_codex_line(&event("2026-11-22T06:00:00Z", 1000, 400, 50), &mut previous).unwrap();
        assert_eq!((first.input, first.cache_read, first.output), (600, 400, 50));
        assert!(parse_codex_line(&event("2026-11-22T06:00:01Z", 1000, 400, 50), &mut previous).is_none());
        let second = parse_codex_line(&event("2026-11-22T06:05:00Z", 1500, 800, 80), &mut previous).unwrap();
        assert_eq!((second.input, second.cache_read, second.output), (100, 400, 30));
        assert!(second.cost().is_none());
        assert!(parse_codex_line(r#"{"type":"turn_context","payload":{"model":"x"}}"#, &mut previous).is_none());
    }
}
