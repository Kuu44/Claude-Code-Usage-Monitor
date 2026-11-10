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
    pub antigravity_claude: Option<UsageData>,
    pub antigravity_argon: Option<UsageData>,
    pub grok: Option<UsageData>,
    pub cursor: Option<UsageData>,
    pub claude_code_auth_required: bool,
    pub codex_auth_required: bool,
    pub antigravity_auth_required: bool,
    pub grok_auth_required: bool,
    pub cursor_auth_required: bool,
}

/// Quota pools share authentication, but never share balances.
#[derive(Clone, Debug, Default)]
pub struct AntigravityUsage {
    pub gemini: Option<UsageData>,
    pub claude: Option<UsageData>,
    pub argon: Option<UsageData>,
}
