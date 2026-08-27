use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AppUsageData, UsageData, UsageSection};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const CURSOR_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const ANTIGRAVITY_CREDENTIAL_TARGET: &str = "gemini:antigravity";
const ANTIGRAVITY_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const CREATE_NO_WINDOW: u32 = 0x08000000;
const PROVIDER_CLIENT_VERSION: &str =
    concat!("claude-code-usage-monitor/", env!("CARGO_PKG_VERSION"));

const MODEL_FALLBACK_CHAIN: &[&str] = &["claude-3-haiku-20240307", "claude-haiku-4-5-20251001"];
static CURSOR_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Clone, Debug)]
pub struct PollFailure {
    pub error: PollError,
    pub data: AppUsageData,
}

impl PollError {
    pub fn requires_login(self) -> bool {
        matches!(
            self,
            Self::AuthRequired | Self::NoCredentials | Self::TokenExpired
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
    Codex,
    Antigravity,
    Grok,
    Cursor,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
    limit_window_seconds: Option<i64>,
    window_minutes: Option<i64>,
}

#[derive(Deserialize)]
struct AntigravityAuthFile {
    token: AntigravityTokenData,
}

#[derive(Deserialize)]
struct AntigravityTokenData {
    access_token: String,
}

#[derive(Deserialize)]
struct AntigravityLoadResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityModelsResponse {
    models: HashMap<String, AntigravityModelInfo>,
}

#[derive(Deserialize)]
struct AntigravityModelInfo {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Deserialize)]
struct AntigravityQuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryResponse {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    buckets: Option<Vec<AntigravityQuotaSummaryBucket>>,
}

#[derive(Clone, Deserialize)]
struct AntigravityQuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrokAuthMode {
    #[serde(alias = "grok")]
    WebLogin,
    #[serde(alias = "oidc")]
    Oidc,
    External,
    ApiKey,
}

#[derive(Clone, Deserialize)]
struct GrokAuthEntry {
    key: String,
    auth_mode: GrokAuthMode,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Clone)]
struct GrokCredentials {
    key: String,
    user_id: String,
}

#[derive(Deserialize)]
struct CursorUsageResponse {
    #[serde(rename = "planUsage", alias = "plan_usage")]
    plan_usage: Option<CursorPlanUsage>,
    #[serde(rename = "billingCycleEnd", alias = "billing_cycle_end")]
    billing_cycle_end: Option<serde_json::Value>,
    #[serde(rename = "planInfo", alias = "plan_info")]
    plan_info: Option<CursorPlanInfo>,
}

#[derive(Deserialize)]
struct CursorPlanUsage {
    #[serde(rename = "totalPercentUsed", alias = "total_percent_used")]
    total_percent_used: Option<serde_json::Value>,
    #[serde(rename = "includedSpend", alias = "included_spend")]
    included_spend: Option<serde_json::Value>,
    limit: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CursorPlanInfo {
    #[serde(rename = "billingCycleEnd", alias = "billing_cycle_end")]
    billing_cycle_end: Option<serde_json::Value>,
}

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

pub fn poll(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
) -> Result<AppUsageData, PollFailure> {
    poll_with(
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
        poll_claude_code,
        poll_codex,
        poll_antigravity,
        poll_grok,
        poll_cursor,
    )
}

fn poll_with(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
    mut poll_claude_code: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_codex: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_antigravity: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_grok: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_cursor: impl FnMut() -> Result<UsageData, PollError>,
) -> Result<AppUsageData, PollFailure> {
    let mut data = AppUsageData::default();
    let mut first_error = None;
    let active_provider_count = show_claude_code as u8
        + show_codex as u8
        + show_antigravity as u8
        + show_grok as u8
        + show_cursor as u8;

    if show_claude_code {
        match poll_claude_code() {
            Ok(claude_code) => data.claude_code = Some(claude_code),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Claude Code usage poll failed: {error:?}"));
                }
                data.claude_code_auth_required = error.requires_login();
                first_error.get_or_insert(error);
            }
        }
    }

    if show_codex {
        match poll_codex() {
            Ok(codex) => data.codex = Some(codex),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Codex usage poll failed: {error:?}"));
                }
                data.codex_auth_required = error.requires_login();
                first_error.get_or_insert(error);
            }
        }
    }

    if show_antigravity {
        match poll_antigravity() {
            Ok(antigravity) => data.antigravity = Some(antigravity),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Antigravity usage poll failed: {error:?}"));
                }
                data.antigravity_auth_required = error.requires_login();
                first_error.get_or_insert(error);
            }
        }
    }

    if show_grok {
        match poll_grok() {
            Ok(grok) => data.grok = Some(grok),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Grok usage poll failed: {error:?}"));
                }
                data.grok_auth_required = error.requires_login();
                first_error.get_or_insert(error);
            }
        }
    }

    if show_cursor {
        match poll_cursor() {
            Ok(cursor) => data.cursor = Some(cursor),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Cursor usage poll failed: {error:?}"));
                }
                data.cursor_auth_required = error.requires_login();
                first_error.get_or_insert(error);
            }
        }
    }

    if data.claude_code.is_none()
        && data.codex.is_none()
        && data.antigravity.is_none()
        && data.grok.is_none()
        && data.cursor.is_none()
    {
        Err(PollFailure {
            error: first_error.unwrap_or(PollError::RequestFailed),
            data,
        })
    } else {
        Ok(data)
    }
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            cli_refresh_codex_token();
            let refreshed = read_codex_credentials().ok_or(PollError::TokenExpired)?;
            fetch_codex_usage(&refreshed.access_token, refreshed.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn poll_antigravity() -> Result<UsageData, PollError> {
    let creds = match read_antigravity_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    fetch_antigravity_usage(&creds.access_token)
}

fn poll_grok() -> Result<UsageData, PollError> {
    let credentials = read_grok_credentials().ok_or(PollError::NoCredentials)?;
    fetch_grok_usage(&credentials)
}

fn poll_cursor() -> Result<UsageData, PollError> {
    let access_token = read_cursor_credentials().ok_or(PollError::AuthRequired)?;
    fetch_cursor_usage(&access_token)
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    loop {
        if !is_token_expired(creds.expires_at) {
            return Ok(creds);
        }

        let source = creds.source.clone();
        cli_refresh_token(&source);

        match read_credentials_from_source(&source) {
            Some(refreshed) if !is_token_expired(refreshed.expires_at) => return Ok(refreshed),
            Some(_) => diagnose::log(format!(
                "credentials from {source:?} still expired after refresh attempt"
            )),
            None => diagnose::log(format!(
                "credentials from {source:?} unavailable after refresh attempt"
            )),
        }

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => return Err(PollError::TokenExpired),
        }
    }
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

fn cli_refresh_codex_token() {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&codex_path).args(args);
        c
    } else if is_ps1 {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        c
    } else {
        let mut c = Command::new(&codex_path);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    match mode {
        CredentialWatchMode::Antigravity => return vec![antigravity_credential_watch_signature()],
        CredentialWatchMode::Codex => return vec![codex_credential_watch_signature()],
        CredentialWatchMode::Grok => return vec![grok_credential_watch_signature()],
        CredentialWatchMode::Cursor => return vec![cursor_credential_watch_signature()],
        CredentialWatchMode::ActiveSource | CredentialWatchMode::AllSources => {}
    }

    let sources = match mode {
        CredentialWatchMode::ActiveSource => read_first_credentials()
            .map(|creds| vec![creds.source])
            .unwrap_or_else(all_known_credential_sources),
        CredentialWatchMode::AllSources => all_known_credential_sources(),
        CredentialWatchMode::Codex
        | CredentialWatchMode::Antigravity
        | CredentialWatchMode::Grok
        | CredentialWatchMode::Cursor => unreachable!(),
    };

    let mut snapshot: CredentialWatchSnapshot = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    if mode == CredentialWatchMode::AllSources {
        snapshot.push(codex_credential_watch_signature());
        snapshot.push(antigravity_credential_watch_signature());
        snapshot.push(grok_credential_watch_signature());
        snapshot.push(cursor_credential_watch_signature());
        snapshot.sort();
        snapshot.dedup();
    }
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then \
                 stat -c 'present|%s|%Y' ~/.claude/.credentials.json; \
                 else echo missing; fi",
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // Try the dedicated usage endpoint first
    match try_usage_endpoint(token)? {
        Some(data) => {
            // If reset timers are missing, fill them in from the Messages API
            if data.session.resets_at.is_none() || data.weekly.resets_at.is_none() {
                if let Ok(fallback) = fetch_usage_via_messages(token) {
                    let mut merged = data;
                    if merged.session.resets_at.is_none() {
                        merged.session.resets_at = fallback.session.resets_at;
                    }
                    if merged.weekly.resets_at.is_none() {
                        merged.weekly.resets_at = fallback.weekly.resets_at;
                    }
                    return Ok(merged);
                }
            }
            return Ok(data);
        }
        None => {}
    }

    // Fall back to Messages API with rate limit headers
    let result = fetch_usage_via_messages(token);
    if result.is_err() {
        diagnose::log("usage endpoint and Messages API fallback both failed");
    }
    result
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "usage endpoint returned auth error status {code}; re-login required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(_) => return Ok(None),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let mut data = UsageData::default();

    if let Some(bucket) = &response.five_hour {
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    Ok(Some(data))
}

fn fetch_usage_via_messages(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    for model in MODEL_FALLBACK_CHAIN {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                diagnose::log(format!(
                    "messages endpoint returned auth error status {code}; re-login required"
                ));
                return Err(PollError::AuthRequired);
            }
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> UsageData {
    let mut data = UsageData::default();

    data.session.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-5h-utilization") * 100.0;
    data.session.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-5h-reset",
    ));

    data.weekly.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-7d-utilization") * 100.0;
    data.weekly.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-7d-reset",
    ));

    let overall_reset = get_header_i64(response, "anthropic-ratelimit-unified-reset");

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        let status = response.header("anthropic-ratelimit-unified-status");
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = unix_to_system_time(overall_reset);
        }
    }

    data
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn fetch_grok_usage(credentials: &GrokCredentials) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let response = match agent
        .get(GROK_BILLING_URL)
        .set("Authorization", &format!("Bearer {}", credentials.key))
        .set("X-XAI-Token-Auth", "xai-grok-cli")
        .set("x-userid", &credentials.user_id)
        .set("x-grok-client-version", PROVIDER_CLIENT_VERSION)
        .set("x-grok-client-mode", "interactive")
        .set("Accept", "application/json")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => return Err(classify_http_status(code)),
        Err(error) => {
            diagnose::log_error("Grok billing request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: serde_json::Value = response.into_json().map_err(|error| {
        diagnose::log_error("unable to parse Grok billing response", error);
        PollError::RequestFailed
    })?;

    grok_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn fetch_cursor_usage(access_token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({});
    let response = match agent
        .post(CURSOR_USAGE_URL)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("Connect-Protocol-Version", "1")
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("x-ghost-mode", "true")
        .set("x-cursor-client-version", PROVIDER_CLIENT_VERSION)
        .set("x-cursor-client-type", "cli")
        .set("x-request-id", &fresh_cursor_request_id())
        .send_json(&body)
    {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => return Err(classify_http_status(code)),
        Err(error) => {
            diagnose::log_error("Cursor usage request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CursorUsageResponse = response.into_json().map_err(|error| {
        diagnose::log_error("unable to parse Cursor usage response", error);
        PollError::RequestFailed
    })?;

    cursor_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn grok_usage_from_response(response: serde_json::Value) -> Option<UsageData> {
    let config = response.get("config")?.as_object()?;
    let period = config.get("currentPeriod")?.as_object()?;
    if period.get("type")?.as_str()? != "USAGE_PERIOD_TYPE_WEEKLY" {
        return None;
    }

    let _start = parse_period_timestamp(period.get("start")?)?;
    let end = parse_period_timestamp(period.get("end")?)?;
    let percentage = match config.get("creditUsagePercent") {
        Some(value) => finite_json_number(value)?.clamp(0.0, 100.0),
        None => 0.0,
    };

    Some(UsageData {
        session: UsageSection::default(),
        weekly: UsageSection {
            percentage,
            resets_at: Some(end),
        },
    })
}

fn cursor_usage_from_response(response: CursorUsageResponse) -> Option<UsageData> {
    let plan = response.plan_usage?;
    let percentage = plan
        .total_percent_used
        .as_ref()
        .and_then(finite_json_number)
        .or_else(|| {
            let included_spend = finite_json_number(plan.included_spend.as_ref()?)?;
            let limit = finite_json_number(plan.limit.as_ref()?)?;
            (limit > 0.0).then_some(included_spend / limit * 100.0)
        })?
        .clamp(0.0, 100.0);

    Some(UsageData {
        session: UsageSection::default(),
        weekly: UsageSection {
            percentage,
            resets_at: response
                .billing_cycle_end
                .as_ref()
                .and_then(parse_cursor_reset_timestamp)
                .or_else(|| {
                    response
                        .plan_info
                        .as_ref()
                        .and_then(|plan| plan.billing_cycle_end.as_ref())
                        .and_then(parse_cursor_reset_timestamp)
                }),
        },
    })
}

fn finite_json_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        serde_json::Value::String(value) => value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

fn parse_period_timestamp(value: &serde_json::Value) -> Option<SystemTime> {
    if let Some(value) = value.as_str() {
        if let Some(parsed) = parse_iso8601(Some(value)) {
            return Some(parsed);
        }
    }

    let value = finite_json_number(value)?;
    if value < 0.0 {
        return None;
    }
    let millis = if value >= 100_000_000_000.0 {
        value as u64
    } else {
        (value * 1000.0) as u64
    };
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn parse_cursor_reset_timestamp(value: &serde_json::Value) -> Option<SystemTime> {
    if finite_json_number(value).is_some_and(|number| number == 0.0) {
        return None;
    }

    parse_period_timestamp(value)
}

fn fresh_cursor_request_id() -> String {
    let sequence = CURSOR_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ccum-{timestamp:x}-{sequence:x}")
}

fn classify_http_status(status: u16) -> PollError {
    if status == 401 || status == 403 {
        PollError::AuthRequired
    } else {
        PollError::RequestFailed
    }
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut data = UsageData::default();

    if let Some(window) = details.primary_window.flatten() {
        if codex_window_is_weekly(&window).unwrap_or(false) {
            data.weekly = codex_section_from_window(&window);
        } else {
            data.session = codex_section_from_window(&window);
        }
    }

    if let Some(window) = details.secondary_window.flatten() {
        if codex_window_is_weekly(&window).unwrap_or(true) {
            data.weekly = codex_section_from_window(&window);
        } else {
            data.session = codex_section_from_window(&window);
        }
    }

    Some(data)
}

fn codex_window_is_weekly(window: &CodexRateLimitWindow) -> Option<bool> {
    window
        .limit_window_seconds
        .map(|seconds| seconds >= 24 * 60 * 60)
        .or_else(|| window.window_minutes.map(|minutes| minutes >= 24 * 60))
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
    }
}

fn antigravity_credential_watch_signature() -> String {
    let Some(content) = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET) else {
        return format!("{ANTIGRAVITY_CREDENTIAL_TARGET}|missing");
    };

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!(
        "{ANTIGRAVITY_CREDENTIAL_TARGET}|present|{}|{}",
        content.len(),
        hasher.finish()
    )
}

fn grok_auth_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".grok").join("auth.json"))
}

fn cursor_auth_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .map(|root| root.join("Cursor").join("auth.json"))
}

fn grok_credential_watch_signature() -> String {
    credential_file_watch_signature("grok", grok_auth_path())
}

fn cursor_credential_watch_signature() -> String {
    credential_file_watch_signature("cursor", cursor_auth_path())
}

fn credential_file_watch_signature(name: &str, path: Option<PathBuf>) -> String {
    let Some(path) = path else {
        return format!("{name}|unavailable");
    };

    match std::fs::metadata(&path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{name}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{name}|missing"),
    }
}

fn fetch_antigravity_usage(token: &str) -> Result<UsageData, PollError> {
    let mut auth_error = false;
    let mut last_error = PollError::RequestFailed;

    for base_url in ANTIGRAVITY_ENDPOINTS {
        match fetch_antigravity_usage_from_endpoint(base_url, token) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => auth_error = true,
            Err(error) => last_error = error,
        }
    }

    if auth_error {
        Err(PollError::AuthRequired)
    } else {
        Err(last_error)
    }
}

fn fetch_antigravity_usage_from_endpoint(
    base_url: &str,
    token: &str,
) -> Result<UsageData, PollError> {
    let project = fetch_antigravity_project(base_url, token)?;
    if let Some(project) = project.as_deref() {
        match fetch_antigravity_quota_summary(base_url, token, project) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => return Err(PollError::AuthRequired),
            Err(error) => diagnose::log(format!(
                "Antigravity retrieveUserQuotaSummary failed, falling back to model quota: {error:?}"
            )),
        }
    }

    let session = fetch_antigravity_model_quota(base_url, token, project.as_deref())?;
    let weekly = UsageSection::default();

    Ok(UsageData { session, weekly })
}

fn fetch_antigravity_project(base_url: &str, token: &str) -> Result<Option<String>, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:loadCodeAssist"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Antigravity loadCodeAssist returned auth error status {code}"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity loadCodeAssist request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityLoadResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Antigravity loadCodeAssist response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(response.project.filter(|project| !project.is_empty()))
}

fn fetch_antigravity_model_quota(
    base_url: &str,
    token: &str,
    project: Option<&str>,
) -> Result<UsageSection, PollError> {
    let agent = build_agent()?;
    let body = match project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };

    let resp = match agent
        .post(&format!("{base_url}/v1internal:fetchAvailableModels"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Antigravity fetchAvailableModels returned auth error status {code}"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity fetchAvailableModels request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityModelsResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity fetchAvailableModels response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    best_antigravity_section(response.models.into_iter().filter_map(|(model, info)| {
        let quota = info.quota_info?;
        if !is_antigravity_display_model(&model) {
            return None;
        }
        antigravity_section_from_quota(quota)
    }))
    .ok_or(PollError::RequestFailed)
}

fn fetch_antigravity_quota_summary(
    base_url: &str,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:retrieveUserQuotaSummary"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuotaSummary request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityQuotaSummaryResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuotaSummary response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    antigravity_usage_from_summary(response).ok_or(PollError::RequestFailed)
}

fn antigravity_section_from_quota(quota: AntigravityQuotaInfo) -> Option<UsageSection> {
    let remaining = quota.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(quota.reset_time.as_deref()),
    })
}

fn antigravity_section_from_summary_bucket(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<UsageSection> {
    let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(bucket.reset_time.as_deref()),
    })
}

fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let mut fallback = None;

    for group in response.groups.unwrap_or_default() {
        let is_gemini = is_antigravity_gemini_summary_group(&group);
        let usage = antigravity_usage_from_summary_group(group);

        if is_gemini && usage.is_some() {
            return usage;
        }

        if fallback.is_none() {
            fallback = usage;
        }
    }

    fallback
}

fn antigravity_usage_from_summary_group(group: AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut data = UsageData::default();
    let mut has_quota = false;

    for bucket in group.buckets.unwrap_or_default() {
        let Some(section) = antigravity_section_from_summary_bucket(&bucket) else {
            continue;
        };

        match bucket.window.as_deref() {
            Some(window) if window.eq_ignore_ascii_case("5h") => {
                data.session = section;
                has_quota = true;
            }
            Some(window) if window.eq_ignore_ascii_case("weekly") => {
                data.weekly = section;
                has_quota = true;
            }
            _ => {}
        }
    }

    has_quota.then_some(data)
}

fn is_antigravity_gemini_summary_group(group: &AntigravityQuotaSummaryGroup) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
        || group
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains("gemini"))
        || group.buckets.as_ref().is_some_and(|buckets| {
            buckets.iter().any(|bucket| {
                bucket
                    .bucket_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini-"))
                    || bucket
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
            })
        })
}

fn best_antigravity_section<I>(sections: I) -> Option<UsageSection>
where
    I: IntoIterator<Item = UsageSection>,
{
    sections.into_iter().max_by(|a, b| {
        a.percentage
            .partial_cmp(&b.percentage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.resets_at.cmp(&b.resets_at))
    })
}

fn is_antigravity_display_model(model: &str) -> bool {
    model.starts_with("gemini")
        || model.starts_with("claude")
        || model.starts_with("gpt")
        || model.starts_with("image")
        || model.starts_with("imagen")
}

fn get_header_f64(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn get_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|s| s.parse::<i64>().ok())
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn codex_credential_watch_signature() -> String {
    credential_file_watch_signature("codex", codex_auth_path())
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens.filter(|tokens| !tokens.access_token.is_empty())
}

fn read_grok_credentials() -> Option<GrokCredentials> {
    let auth_path = grok_auth_path()?;
    let content = std::fs::read_to_string(&auth_path).ok()?;
    let auth: BTreeMap<String, GrokAuthEntry> = serde_json::from_str(&content).ok()?;
    select_grok_credentials(auth)
}

fn select_grok_credentials(entries: BTreeMap<String, GrokAuthEntry>) -> Option<GrokCredentials> {
    entries
        .into_iter()
        .filter(|(_, entry)| grok_entry_is_usable(entry))
        .fold(None::<(String, GrokAuthEntry)>, |best, candidate| {
            let candidate_expires_at = candidate
                .1
                .expires_at
                .as_deref()
                .and_then(parse_grok_expiry);
            let should_replace = best
                .as_ref()
                .map(|(_, entry)| {
                    let best_expires_at = entry.expires_at.as_deref().and_then(parse_grok_expiry);
                    candidate_expires_at > best_expires_at
                })
                .unwrap_or(true);

            if should_replace {
                Some(candidate)
            } else {
                best
            }
        })
        .map(|(_, entry)| GrokCredentials {
            key: entry.key,
            user_id: entry.user_id,
        })
}

fn grok_entry_is_usable(entry: &GrokAuthEntry) -> bool {
    let first_party_auth = matches!(entry.auth_mode, GrokAuthMode::Oidc | GrokAuthMode::External);
    let has_refresh_token = entry
        .refresh_token
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_first_party_issuer = entry.oidc_issuer.as_deref().is_some_and(is_xai_grok_issuer);
    let has_future_expiry = entry
        .expires_at
        .as_deref()
        .and_then(parse_grok_expiry)
        .is_some_and(|expires_at| expires_at > SystemTime::now());

    !entry.key.trim().is_empty()
        && !entry.user_id.trim().is_empty()
        && first_party_auth
        && has_refresh_token
        && has_first_party_issuer
        && has_future_expiry
}

fn is_xai_grok_issuer(value: &str) -> bool {
    matches!(value, "https://auth.x.ai" | "http://localhost:22255")
}

fn parse_grok_expiry(value: &str) -> Option<SystemTime> {
    if value.is_empty() || value.trim() != value {
        return None;
    }

    let has_timezone = value.ends_with('Z')
        || value
            .get(10..)
            .is_some_and(|suffix| suffix.contains('+') || suffix.contains('-'));
    has_timezone.then(|| parse_iso8601(Some(value))).flatten()
}

fn read_cursor_credentials() -> Option<String> {
    let auth_path = cursor_auth_path()?;
    let content = std::fs::read_to_string(&auth_path).ok()?;
    let auth: serde_json::Value = serde_json::from_str(&content).ok()?;
    auth.get("accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
}

fn read_antigravity_credentials() -> Option<AntigravityTokenData> {
    let content = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET)?;
    let auth: AntigravityAuthFile = serde_json::from_str(&content).ok()?;
    if auth.token.access_token.is_empty() {
        None
    } else {
        Some(auth.token)
    }
}

fn read_windows_generic_credential(target: &str) -> Option<String> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();

    let ok = unsafe {
        CredReadW(
            target_wide.as_mut_ptr(),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
    };

    if ok == 0 || credential.is_null() {
        diagnose::log(format!(
            "unable to read Windows generic credential target {target}"
        ));
        return None;
    }

    let result = unsafe {
        let cred = &*credential;
        if cred.credential_blob_size == 0 || cred.credential_blob.is_null() {
            CredFree(credential as *mut c_void);
            return None;
        }
        let bytes =
            std::slice::from_raw_parts(cred.credential_blob, cred.credential_blob_size as usize);
        let text = String::from_utf8(bytes.to_vec()).ok();
        CredFree(credential as *mut c_void);
        text
    };

    result
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?;
    if s.len() < 10 || !s.is_char_boundary(10) {
        return None;
    }
    // Strip the timezone suffix before parsing the calendar portion. The API
    // returns formats such as "2026-03-05T08:00:00.321598+00:00" and RFC3339
    // also permits negative offsets.
    let (datetime_part, offset_seconds) = if let Some(datetime) = s.strip_suffix('Z') {
        (datetime, 0_i64)
    } else if let Some(index) = s[10..].find('+') {
        let index = index + 10;
        (&s[..index], parse_timezone_offset(&s[index..])?)
    } else if let Some(index) = s[10..].find('-') {
        let index = index + 10;
        (&s[..index], -parse_timezone_offset(&s[index..])?)
    } else {
        (s, 0)
    };

    // Try parsing with and without fractional seconds
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    for fmt in &formats {
        if let Ok(secs) = parse_datetime_to_unix(datetime_part, fmt) {
            let secs = if offset_seconds >= 0 {
                secs.saturating_sub(offset_seconds as u64)
            } else {
                secs.saturating_add(offset_seconds.unsigned_abs())
            };
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    None
}

fn parse_timezone_offset(value: &str) -> Option<i64> {
    let value = value.strip_prefix(['+', '-'])?;
    let (hours, minutes) = value.split_once(':')?;
    let hours = hours.parse::<i64>().ok()?;
    let minutes = minutes.parse::<i64>().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 3600 + minutes * 60)
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str, _fmt: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;
    if year < 1970 || !(1..=12).contains(&month) {
        return Err(());
    }

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;
    if hour >= 24 || min >= 60 || sec >= 60 {
        return Err(());
    }

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let days_in_month = month_days[month as usize] + u64::from(month == 2 && is_leap(year));
    if day == 0 || day > days_in_month {
        return Err(());
    }
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text
pub fn format_line(section: &UsageSection, strings: Strings) -> String {
    let pct = format!("{:.0}%", section.percentage);
    let cd = format_countdown(section.resets_at, strings);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}{}", strings.day_suffix)
    } else if total_hours >= 1 {
        format!("{total_hours}{}", strings.hour_suffix)
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.claude_code.as_ref().is_some_and(is_past_reset)
        || data.codex.as_ref().is_some_and(is_past_reset)
        || data.antigravity.as_ref().is_some_and(is_past_reset)
        || data.grok.as_ref().is_some_and(is_past_reset)
        || data.cursor.as_ref().is_some_and(is_past_reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_with_session_percent(percentage: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                percentage,
                resets_at: None,
            },
            weekly: UsageSection::default(),
        }
    }

    #[test]
    fn codex_weekly_only_window_stays_in_weekly_slot() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 32,
                        "reset_at": 2000000000,
                        "limit_window_seconds": 604800
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .unwrap();

        let usage = codex_usage_from_response(response).unwrap();

        assert!(usage.session.resets_at.is_none());
        assert_eq!(usage.weekly.percentage, 32.0);
        assert!(usage.weekly.resets_at.is_some());
    }

    #[test]
    fn grok_weekly_usage_maps_to_bottom_slot_and_reset() {
        let usage = grok_usage_from_response(serde_json::json!({
            "config": {
                "creditUsagePercent": 37.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-24T00:00:00Z",
                    "end": "2026-08-31T00:00:00Z"
                }
            }
        }))
        .expect("valid Grok weekly config should parse");

        assert_eq!(usage.session.percentage, 0.0);
        assert!(usage.session.resets_at.is_none());
        assert_eq!(usage.weekly.percentage, 37.5);
        assert_eq!(
            usage.weekly.resets_at,
            Some(parse_iso8601(Some("2026-08-31T00:00:00Z")).unwrap())
        );
    }

    #[test]
    fn grok_omitted_percentage_is_zero_but_invalid_period_is_unavailable() {
        let usage = grok_usage_from_response(serde_json::json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-24T00:00:00Z",
                    "end": "2026-08-31T00:00:00Z"
                }
            }
        }))
        .expect("valid Grok weekly config without percentage should parse");
        assert_eq!(usage.weekly.percentage, 0.0);

        assert!(grok_usage_from_response(serde_json::json!({
            "config": {
                "creditUsagePercent": 20,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "start": "2026-08-01T00:00:00Z",
                    "end": "2026-09-01T00:00:00Z"
                }
            }
        }))
        .is_none());
        assert!(grok_usage_from_response(serde_json::json!({
            "config": { "creditUsagePercent": 20 }
        }))
        .is_none());
        assert!(grok_usage_from_response(serde_json::json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "not-a-timestamp",
                    "end": "2026-08-31T00:00:00Z"
                }
            }
        }))
        .is_none());
    }

    #[test]
    fn grok_percentage_is_clamped_and_http_auth_status_is_classified() {
        let usage = grok_usage_from_response(serde_json::json!({
            "config": {
                "creditUsagePercent": 150,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-24T00:00:00Z",
                    "end": "2026-08-31T00:00:00Z"
                }
            }
        }))
        .unwrap();
        assert_eq!(usage.weekly.percentage, 100.0);
        assert_eq!(classify_http_status(401), PollError::AuthRequired);
        assert_eq!(classify_http_status(403), PollError::AuthRequired);
        assert_eq!(classify_http_status(500), PollError::RequestFailed);
    }

    fn grok_auth_fixture(
        auth_mode: &str,
        issuer: Option<&str>,
        expires_at: Option<&str>,
        key: &str,
        user_id: &str,
        refresh_token: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "key": key,
            "auth_mode": auth_mode,
            "create_time": "2026-01-01T00:00:00Z",
            "user_id": user_id,
            "email": "tester@example.invalid",
            "first_name": "Test",
            "last_name": "User",
            "profile_image_asset_id": null,
            "principal_type": null,
            "principal_id": null,
            "team_id": null,
            "team_name": null,
            "team_role": null,
            "organization_id": null,
            "organization_name": null,
            "organization_role": null,
            "user_blocked_reason": null,
            "team_blocked_reasons": [],
            "coding_data_retention_opt_out": true,
            "has_grok_code_access": null,
            "refresh_token": refresh_token,
            "expires_at": expires_at,
            "oidc_issuer": issuer,
            "oidc_client_id": "client-id"
        })
    }

    #[test]
    fn grok_credentials_choose_latest_future_first_party_record_without_logging_values() {
        let mut records = serde_json::Map::new();
        records.insert(
            "aaa-stale".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2020-01-01T00:00:00Z"),
                "stale-key",
                "stale-user",
                Some("stale-refresh"),
            ),
        );
        records.insert(
            "bbb-tie-first".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2099-06-01T00:00:00Z"),
                "tie-first-key",
                "tie-first-user",
                Some("tie-first-refresh"),
            ),
        );
        records.insert(
            "zzz-tie-later".to_string(),
            grok_auth_fixture(
                "external",
                Some("https://auth.x.ai"),
                Some("2099-06-01T00:00:00Z"),
                "tie-later-key",
                "tie-later-user",
                Some("tie-later-refresh"),
            ),
        );

        let auth: BTreeMap<String, GrokAuthEntry> =
            serde_json::from_value(serde_json::Value::Object(records)).unwrap();
        let credentials =
            select_grok_credentials(auth).expect("first-party OIDC record should be selected");
        assert_eq!(credentials.key, "tie-first-key");
        assert_eq!(credentials.user_id, "tie-first-user");
    }

    #[test]
    fn grok_credentials_reject_non_xai_auth_modes_issuers_and_malformed_fields() {
        let mut records = serde_json::Map::new();
        records.insert(
            "customer-oidc".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://idp.example"),
                Some("2026-12-01T00:00:00Z"),
                "customer-oidc-key",
                "customer-oidc-user",
                Some("customer-oidc-refresh"),
            ),
        );
        records.insert(
            "customer-external".to_string(),
            grok_auth_fixture(
                "external",
                Some("https://idp.example"),
                Some("2026-12-02T00:00:00Z"),
                "customer-external-key",
                "customer-external-user",
                Some("customer-external-refresh"),
            ),
        );
        records.insert(
            "api-key".to_string(),
            grok_auth_fixture(
                "api_key",
                Some("https://auth.x.ai"),
                Some("2026-12-03T00:00:00Z"),
                "api-key",
                "api-user",
                Some("api-refresh"),
            ),
        );
        records.insert(
            "web-login".to_string(),
            grok_auth_fixture(
                "grok",
                Some("https://auth.x.ai"),
                Some("2026-12-04T00:00:00Z"),
                "web-login-key",
                "web-login-user",
                Some("web-login-refresh"),
            ),
        );
        records.insert(
            "empty-issuer".to_string(),
            grok_auth_fixture(
                "oidc",
                Some(""),
                Some("2026-12-05T00:00:00Z"),
                "empty-issuer-key",
                "empty-issuer-user",
                Some("empty-issuer-refresh"),
            ),
        );
        records.insert(
            "missing-issuer".to_string(),
            grok_auth_fixture(
                "oidc",
                None,
                Some("2026-12-06T00:00:00Z"),
                "missing-issuer-key",
                "missing-issuer-user",
                Some("missing-issuer-refresh"),
            ),
        );
        records.insert(
            "malformed-expiry".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("not-a-timestamp"),
                "malformed-expiry-key",
                "malformed-expiry-user",
                Some("malformed-expiry-refresh"),
            ),
        );
        records.insert(
            "empty-expiry".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some(""),
                "empty-expiry-key",
                "empty-expiry-user",
                Some("empty-expiry-refresh"),
            ),
        );
        records.insert(
            "empty-refresh".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2099-12-07T00:00:00Z"),
                "empty-refresh-key",
                "empty-refresh-user",
                Some(""),
            ),
        );
        records.insert(
            "missing-expiry".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                None,
                "missing-expiry-key",
                "missing-expiry-user",
                Some("missing-expiry-refresh"),
            ),
        );

        let auth: BTreeMap<String, GrokAuthEntry> =
            serde_json::from_value(serde_json::Value::Object(records)).unwrap();
        assert!(select_grok_credentials(auth).is_none());
    }

    #[test]
    fn grok_credentials_ignore_expired_records_and_reject_all_expired_maps() {
        let mut mixed_records = serde_json::Map::new();
        mixed_records.insert(
            "expired".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2020-01-01T00:00:00Z"),
                "expired-key",
                "expired-user",
                Some("expired-refresh"),
            ),
        );
        mixed_records.insert(
            "valid".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2099-01-01T00:00:00Z"),
                "valid-key",
                "valid-user",
                Some("valid-refresh"),
            ),
        );
        let mixed_auth: BTreeMap<String, GrokAuthEntry> =
            serde_json::from_value(serde_json::Value::Object(mixed_records)).unwrap();
        let credentials =
            select_grok_credentials(mixed_auth).expect("future credentials should be selected");
        assert_eq!(credentials.key, "valid-key");

        let mut expired_records = serde_json::Map::new();
        expired_records.insert(
            "expired-a".to_string(),
            grok_auth_fixture(
                "oidc",
                Some("https://auth.x.ai"),
                Some("2020-01-01T00:00:00Z"),
                "expired-a-key",
                "expired-a-user",
                Some("expired-a-refresh"),
            ),
        );
        expired_records.insert(
            "expired-b".to_string(),
            grok_auth_fixture(
                "external",
                Some("https://auth.x.ai"),
                Some("2021-01-01T00:00:00Z"),
                "expired-b-key",
                "expired-b-user",
                Some("expired-b-refresh"),
            ),
        );
        let expired_auth: BTreeMap<String, GrokAuthEntry> =
            serde_json::from_value(serde_json::Value::Object(expired_records)).unwrap();
        assert!(select_grok_credentials(expired_auth).is_none());
    }

    #[test]
    fn cursor_camel_case_uses_total_percent_and_epoch_millis_reset() {
        let response: CursorUsageResponse = serde_json::from_value(serde_json::json!({
            "planUsage": {
                "totalPercentUsed": 42.5
            },
            "billingCycleEnd": 1788134400000_i64
        }))
        .unwrap();
        let usage = cursor_usage_from_response(response).unwrap();

        assert_eq!(usage.session.percentage, 0.0);
        assert_eq!(usage.weekly.percentage, 42.5);
        assert_eq!(
            usage.weekly.resets_at,
            Some(UNIX_EPOCH + Duration::from_millis(1788134400000))
        );
    }

    #[test]
    fn cursor_zero_top_level_reset_falls_back_to_plan_info() {
        let response: CursorUsageResponse = serde_json::from_value(serde_json::json!({
            "planUsage": {
                "totalPercentUsed": 42.5
            },
            "billingCycleEnd": 0,
            "planInfo": {
                "billingCycleEnd": "2026-09-15T00:00:00Z"
            }
        }))
        .unwrap();
        let usage = cursor_usage_from_response(response).unwrap();

        assert_eq!(
            usage.weekly.resets_at,
            Some(parse_iso8601(Some("2026-09-15T00:00:00Z")).unwrap())
        );
    }

    #[test]
    fn cursor_snake_case_falls_back_to_spend_ratio_and_accepts_string_resets() {
        let response: CursorUsageResponse = serde_json::from_value(serde_json::json!({
            "plan_usage": {
                "included_spend": 25,
                "limit": 100
            },
            "billing_cycle_end": "2026-09-01T00:00:00Z"
        }))
        .unwrap();
        let usage = cursor_usage_from_response(response).unwrap();

        assert_eq!(usage.weekly.percentage, 25.0);
        assert_eq!(
            usage.weekly.resets_at,
            Some(parse_iso8601(Some("2026-09-01T00:00:00Z")).unwrap())
        );

        let numeric_string: CursorUsageResponse = serde_json::from_value(serde_json::json!({
            "planUsage": {
                "totalPercentUsed": "10"
            },
            "billingCycleEnd": "1788134400000"
        }))
        .unwrap();
        let numeric_usage = cursor_usage_from_response(numeric_string).unwrap();
        assert_eq!(numeric_usage.weekly.percentage, 10.0);
        assert_eq!(
            numeric_usage.weekly.resets_at,
            Some(UNIX_EPOCH + Duration::from_millis(1788134400000))
        );

        let fallback: CursorUsageResponse = serde_json::from_value(serde_json::json!({
            "planUsage": { "totalPercentUsed": 12 },
            "planInfo": { "billingCycleEnd": "2026-09-15T00:00:00Z" }
        }))
        .unwrap();
        let fallback_usage = cursor_usage_from_response(fallback).unwrap();
        assert_eq!(
            fallback_usage.weekly.resets_at,
            Some(parse_iso8601(Some("2026-09-15T00:00:00Z")).unwrap())
        );
    }

    #[test]
    fn cursor_missing_plan_usage_is_unavailable() {
        let response: CursorUsageResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(cursor_usage_from_response(response).is_none());
        assert_eq!(classify_http_status(401), PollError::AuthRequired);
    }

    #[test]
    fn rfc3339_negative_offset_reset_is_accepted() {
        assert_eq!(
            parse_iso8601(Some("2026-09-01T00:00:00-07:00")),
            Some(UNIX_EPOCH + Duration::from_secs(1788246000))
        );
    }

    #[test]
    fn claude_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            false,
            false,
            || Err(PollError::TokenExpired),
            || Ok(usage_with_session_percent(42.0)),
            || unreachable!("antigravity is disabled"),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(data.claude_code.is_none());
        assert!(data.claude_code_auth_required);
        assert!(!data.codex_auth_required);
        assert!(!data.antigravity_auth_required);
        assert_eq!(data.codex.unwrap().session.percentage, 42.0);
    }

    #[test]
    fn claude_request_failure_does_not_request_login() {
        let data = poll_with(
            true,
            true,
            false,
            false,
            false,
            || Err(PollError::RequestFailed),
            || Ok(usage_with_session_percent(42.0)),
            || unreachable!("antigravity is disabled"),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(!data.claude_code_auth_required);
    }

    #[test]
    fn codex_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            false,
            false,
            || Ok(usage_with_session_percent(64.0)),
            || Err(PollError::RequestFailed),
            || unreachable!("antigravity is disabled"),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("claude data should keep the poll successful");

        assert_eq!(data.claude_code.unwrap().session.percentage, 64.0);
        assert!(data.codex.is_none());
        assert!(!data.claude_code_auth_required);
        assert!(!data.codex_auth_required);
        assert!(!data.antigravity_auth_required);
    }

    #[test]
    fn codex_auth_failure_requests_only_codex_login() {
        let data = poll_with(
            true,
            true,
            false,
            false,
            false,
            || Ok(usage_with_session_percent(64.0)),
            || Err(PollError::TokenExpired),
            || unreachable!("antigravity is disabled"),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("claude data should keep the poll successful");

        assert!(!data.claude_code_auth_required);
        assert!(data.codex_auth_required);
        assert!(!data.antigravity_auth_required);
    }

    #[test]
    fn returns_first_error_when_no_enabled_provider_succeeds() {
        let failure = poll_with(
            true,
            true,
            true,
            false,
            false,
            || Err(PollError::AuthRequired),
            || Err(PollError::RequestFailed),
            || Err(PollError::NoCredentials),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect_err("all-provider failure should return an error");

        assert_eq!(failure.error, PollError::AuthRequired);
        assert!(failure.data.claude_code_auth_required);
        assert!(!failure.data.codex_auth_required);
        assert!(failure.data.antigravity_auth_required);
    }

    #[test]
    fn antigravity_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            false,
            true,
            true,
            false,
            false,
            || unreachable!("claude code is disabled"),
            || Ok(usage_with_session_percent(42.0)),
            || Err(PollError::NoCredentials),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(data.antigravity.is_none());
        assert!(!data.claude_code_auth_required);
        assert!(!data.codex_auth_required);
        assert!(data.antigravity_auth_required);
        assert_eq!(data.codex.unwrap().session.percentage, 42.0);
    }

    #[test]
    fn antigravity_request_failure_does_not_request_login() {
        let data = poll_with(
            false,
            true,
            true,
            false,
            false,
            || unreachable!("claude code is disabled"),
            || Ok(usage_with_session_percent(42.0)),
            || Err(PollError::RequestFailed),
            || unreachable!("grok is disabled"),
            || unreachable!("cursor is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(!data.antigravity_auth_required);
    }

    #[test]
    fn grok_and_cursor_failures_are_isolated_and_auth_aware() {
        let data = poll_with(
            true,
            false,
            false,
            true,
            true,
            || Ok(usage_with_session_percent(64.0)),
            || unreachable!("codex is disabled"),
            || unreachable!("antigravity is disabled"),
            || Err(PollError::AuthRequired),
            || Err(PollError::RequestFailed),
        )
        .expect("Claude data should keep the poll successful");

        assert_eq!(data.claude_code.unwrap().session.percentage, 64.0);
        assert!(data.grok.is_none());
        assert!(data.cursor.is_none());
        assert!(data.grok_auth_required);
        assert!(!data.cursor_auth_required);
    }

    #[test]
    fn antigravity_summary_prefers_gemini_group() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "groups": [
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "window": "weekly",
                                "resetTime": "2026-06-20T18:32:02Z",
                                "remainingFraction": 1
                            },
                            {
                                "bucketId": "3p-5h",
                                "window": "5h",
                                "resetTime": "2026-06-13T23:32:02Z",
                                "remainingFraction": 1
                            }
                        ]
                    },
                    {
                        "displayName": "Gemini Models",
                        "description": "Models within this group: Gemini Flash, Gemini Pro",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit",
                                "window": "weekly",
                                "resetTime": "2026-06-20T17:08:54Z",
                                "remainingFraction": 0.99304295
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit",
                                "window": "5h",
                                "resetTime": "2026-06-13T22:08:54Z",
                                "remainingFraction": 0.9582575
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("summary response should deserialize");

        let usage =
            antigravity_usage_from_summary(response).expect("Gemini quota should be selected");

        assert!((usage.weekly.percentage - 0.695705).abs() < 0.000001);
        assert!((usage.session.percentage - 4.17425).abs() < 0.000001);
        assert!(usage.weekly.resets_at.is_some());
        assert!(usage.session.resets_at.is_some());
    }
}
