use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
    pub grok: Option<UsageData>,
    pub cursor: Option<UsageData>,
    pub claude_code_auth_required: bool,
    pub codex_auth_required: bool,
    pub antigravity_auth_required: bool,
    pub grok_auth_required: bool,
    pub cursor_auth_required: bool,
}
