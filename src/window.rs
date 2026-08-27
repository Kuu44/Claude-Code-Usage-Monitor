use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Accessibility::{
    HCF_HIGHCONTRASTON, HIGHCONTRASTW, HWINEVENTHOOK, MSAAMENUINFO, MSAA_MENU_SIG,
};
use windows::Win32::UI::Controls::{
    CloseThemeData, GetThemeColor, OpenThemeData, DRAWITEMSTRUCT, MEASUREITEMSTRUCT,
    MENU_POPUPBACKGROUND, ODS_CHECKED, ODS_DISABLED, ODS_FOCUS, ODS_GRAYED, ODS_HOTLIGHT,
    ODS_NOFOCUSRECT, ODS_SELECTED, ODT_MENU, TMT_FILLCOLOR,
};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::AppUsageData;
use crate::native_interop::{
    self, Color, Provider, ProviderColorRole, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL,
    TIMER_UPDATE_CHECK, WM_APP_TRAY, WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Which side of the taskbar the widget anchors to. `tray_offset` is measured
/// from the anchor side: rightwards from the taskbar's left edge when `Left`,
/// leftwards from the tray area when `Right`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TaskbarSide {
    Left,
    #[default]
    Right,
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    antigravity_session_percent: f64,
    antigravity_session_text: String,
    antigravity_weekly_percent: f64,
    antigravity_weekly_text: String,
    grok_session_percent: f64,
    grok_session_text: String,
    grok_weekly_percent: f64,
    grok_weekly_text: String,
    cursor_session_percent: f64,
    cursor_session_text: String,
    cursor_weekly_percent: f64,
    cursor_weekly_text: String,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
    claude_code_auth_required: bool,
    codex_auth_required: bool,
    antigravity_auth_required: bool,
    grok_auth_required: bool,
    cursor_auth_required: bool,

    data: Option<AppUsageData>,

    poll_interval_ms: u32,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,

    taskbar_index: usize,
    taskbar_side: TaskbarSide,
    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_client_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
    shutdown_requested: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

#[derive(Clone, Copy)]
enum LoginProvider {
    ClaudeCode,
    Codex,
    Antigravity,
    Grok,
    Cursor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoginCommand {
    executable: String,
    args: Vec<String>,
    provider_name: &'static str,
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_SIDE_LEFT: u16 = 32;
const IDM_SIDE_RIGHT: u16 = 33;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_LANG_RUSSIAN: u16 = 49;
const IDM_LANG_PORTUGUESE_BRAZIL: u16 = 50;
const IDM_LANG_SIMPLIFIED_CHINESE: u16 = 51;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_MODEL_ANTIGRAVITY: u16 = 62;
const IDM_MODEL_GROK: u16 = 63;
const IDM_MODEL_CURSOR: u16 = 64;
const IDM_LOGIN_CLAUDE_CODE: u16 = 80;
const IDM_LOGIN_CODEX: u16 = 81;
const IDM_LOGIN_ANTIGRAVITY: u16 = 82;
const IDM_LOGIN_GROK: u16 = 83;
const IDM_LOGIN_CURSOR: u16 = 84;
// Dynamic range: IDM_MONITOR_BASE + taskbar index, one item per detected taskbar.
const IDM_MONITOR_BASE: u16 = 100;
const IDM_MONITOR_MAX: u16 = 131;

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

/// How often the watchdog thread polls for an explorer.exe restart (which
/// recreates the taskbar and wipes our tray-icon registration).
const TASKBAR_WATCH_INTERVAL_SECS: u64 = 2;

static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);
const CREATE_NEW_CONSOLE: u32 = 0x00000010;

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

/// Spacing below which two relaunches are treated as a storm (e.g. explorer.exe
/// crash-looping); when detected we back off instead of spawning in a tight loop.
const RELAUNCH_THROTTLE_SECS: u64 = 10;
const RELAUNCH_BACKOFF_SECS: u64 = 30;
/// Environment flag set on a relaunched child so it waits for the previous
/// instance's single-instance mutex instead of exiting immediately.
const ENV_RELAUNCH: &str = "CCUM_RELAUNCH";
/// Unix timestamp (seconds) of the relaunch that spawned this process, passed to
/// the child so it can detect a relaunch storm.
const ENV_LAST_RELAUNCH_UNIX: &str = "CCUM_LAST_RELAUNCH_UNIX";

/// Relaunch the widget as a fresh process after explorer.exe has restarted.
///
/// When the shell restarts it destroys our embedded child window outright (the
/// window is gone, not merely orphaned - `IsWindow` returns false) and leaves
/// the UI thread parked in `GetMessage` with no window to recreate in place.
/// Spawning a clean new process - which re-embeds into the freshly created
/// taskbar - and exiting this one is the robust recovery. The child is flagged
/// via `ENV_RELAUNCH` so it waits for this instance's single-instance mutex to
/// be released before taking over (see the guard in `run`).
fn relaunch_self() {
    // Back off if we are relaunching very soon after the relaunch that spawned
    // us: that signals the shell is crash-looping, not a one-off restart.
    let now = now_unix_secs();
    let last = std::env::var(ENV_LAST_RELAUNCH_UNIX)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if last != 0 && now.saturating_sub(last) < RELAUNCH_THROTTLE_SECS {
        diagnose::log("relaunch storm detected; backing off before relaunching");
        std::thread::sleep(Duration::from_secs(RELAUNCH_BACKOFF_SECS));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            diagnose::log_error("watchdog: unable to resolve current executable", error);
            return;
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(exe)
        .args(&args)
        .env(ENV_RELAUNCH, "1")
        .env(ENV_LAST_RELAUNCH_UNIX, now.to_string())
        .spawn()
    {
        Ok(_) => {
            diagnose::log("watchdog: relaunched fresh instance, exiting old one");
            std::process::exit(0);
        }
        Err(error) => {
            diagnose::log_error("watchdog: unable to spawn relaunched instance", error);
        }
    }
}

/// Detect explorer.exe restarts and recover from them.
///
/// Once explorer destroys the taskbar, our embedded child window is destroyed
/// and the UI message loop is dead, so recovery cannot happen in-process. This
/// dedicated thread (independent of the dead message loop) polls the taskbar
/// handle and, when it changes, relaunches the widget as a fresh process.
fn spawn_taskbar_watchdog() {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
        let stored = {
            let state = lock_state();
            state
                .as_ref()
                .and_then(|s| s.taskbar_hwnd.map(|taskbar| (taskbar, s.hwnd.to_hwnd())))
        };
        // Only relevant once we have embedded into a taskbar at least once.
        let Some((old_taskbar, widget)) = stored else {
            continue;
        };
        let taskbars = native_interop::find_taskbars();
        let taskbar_exists = taskbars.iter().any(|taskbar| taskbar.hwnd == old_taskbar);
        let widget_exists = native_interop::window_exists(widget);
        if !taskbars.is_empty() && taskbar_recovery_needed(widget_exists, taskbar_exists) {
            let new = taskbars[0].hwnd;
            diagnose::log(format!(
                "watchdog: widget/taskbar changed widget_exists={widget_exists} old={:?} new={:?} -> relaunching",
                old_taskbar.0, new.0
            ));
            relaunch_self();
        }
    });
}

fn taskbar_recovery_needed(widget_exists: bool, taskbar_exists: bool) -> bool {
    !widget_exists || !taskbar_exists
}

fn should_wait_for_taskbar_recovery(embedded: bool, shutdown_requested: bool) -> bool {
    embedded && !shutdown_requested
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default)]
    taskbar_index: usize,
    #[serde(default)]
    taskbar_side: TaskbarSide,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    show_antigravity: bool,
    #[serde(default)]
    show_grok: bool,
    #[serde(default)]
    show_cursor: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            taskbar_index: 0,
            taskbar_side: TaskbarSide::default(),
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            show_grok: false,
            show_cursor: false,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
}

fn enforce_provider_invariant(settings: &mut SettingsFile) {
    if !settings.show_claude_code
        && !settings.show_codex
        && !settings.show_antigravity
        && !settings.show_grok
        && !settings.show_cursor
    {
        settings.show_claude_code = true;
    }
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    let mut settings: SettingsFile = serde_json::from_str(&content).unwrap_or_default();
    enforce_provider_invariant(&mut settings);
    settings
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            taskbar_index: s.taskbar_index,
            taskbar_side: s.taskbar_side,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            show_antigravity: s.show_antigravity,
            show_grok: s.show_grok,
            show_cursor: s.show_cursor,
        });
    }
}

fn tray_icon_data_from_state() -> Vec<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: s
                        .data
                        .as_ref()
                        .and_then(|data| data.claude_code.as_ref())
                        .map(|usage| usage.session.percentage),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().claude_code_model,
                        s.session_text,
                        s.weekly_text
                    ),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: s
                        .data
                        .as_ref()
                        .and_then(|data| data.codex.as_ref())
                        .map(|usage| usage.session.percentage),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().codex_model,
                        s.codex_session_text,
                        s.codex_weekly_text
                    ),
                });
            }
            if s.show_antigravity {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Antigravity,
                    percent: s
                        .data
                        .as_ref()
                        .and_then(|data| data.antigravity.as_ref())
                        .map(|usage| usage.session.percentage),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().antigravity_model,
                        s.antigravity_session_text,
                        s.antigravity_weekly_text
                    ),
                });
            }
            if s.show_grok {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Grok,
                    percent: s
                        .data
                        .as_ref()
                        .and_then(|data| data.grok.as_ref())
                        .map(|usage| usage.weekly.percentage),
                    tooltip: format!(
                        "{} {}: {}",
                        s.language.strings().grok_model,
                        s.language.strings().weekly_window,
                        s.grok_weekly_text
                    ),
                });
            }
            if s.show_cursor {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Cursor,
                    percent: s
                        .data
                        .as_ref()
                        .and_then(|data| data.cursor.as_ref())
                        .map(|usage| usage.weekly.percentage),
                    tooltip: format!(
                        "{} {}: {}",
                        s.language.strings().cursor_model,
                        s.language.strings().monthly_window,
                        s.cursor_weekly_text
                    ),
                });
            }
            icons
        }
        Some(s) => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: None,
                    tooltip: s.language.strings().window_title.to_string(),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: None,
                    tooltip: s.language.strings().codex_window_title.to_string(),
                });
            }
            if s.show_antigravity {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Antigravity,
                    percent: None,
                    tooltip: s.language.strings().antigravity_window_title.to_string(),
                });
            }
            if s.show_grok {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Grok,
                    percent: None,
                    tooltip: format!(
                        "{} ({})",
                        s.language.strings().grok_window_title,
                        s.language.strings().weekly_only
                    ),
                });
            }
            if s.show_cursor {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Cursor,
                    percent: None,
                    tooltip: format!(
                        "{} ({})",
                        s.language.strings().cursor_window_title,
                        s.language.strings().monthly_only
                    ),
                });
            }
            icons
        }
        None => Vec::new(),
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icons = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons);
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn attach_to_taskbar(hwnd: HWND, requested_index: usize) -> bool {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        diagnose::log("taskbar not found; using fallback popup window");
        return false;
    }

    let index = requested_index.min(taskbars.len().saturating_sub(1));
    let taskbar = taskbars[index];
    diagnose::log(format!(
        "taskbar selected index={index} count={} hwnd={:?} rect=({}, {}, {}, {})",
        taskbars.len(),
        taskbar.hwnd,
        taskbar.rect.left,
        taskbar.rect.top,
        taskbar.rect.right,
        taskbar.rect.bottom
    ));

    let old_hook = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| s.win_event_hook.take())
    };
    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook);
    }

    native_interop::embed_in_taskbar(hwnd, taskbar.hwnd);

    let tray_notify = native_interop::find_child_window(taskbar.hwnd, "TrayNotifyWnd");
    if tray_notify.is_some() {
        diagnose::log("TrayNotifyWnd found");
    } else {
        diagnose::log("TrayNotifyWnd not found");
    }

    let hook = tray_notify.and_then(|tray_hwnd| {
        let thread_id = native_interop::get_window_thread_id(tray_hwnd);
        native_interop::set_tray_event_hook(thread_id, on_tray_location_changed)
    });
    if hook.is_some() {
        diagnose::log("tray event hook installed");
    } else {
        diagnose::log("tray event hook could not be installed");
    }

    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.taskbar_hwnd = Some(taskbar.hwnd);
        s.tray_notify_hwnd = tray_notify;
        s.win_event_hook = hook;
        s.taskbar_index = index;
        s.embedded = true;
    }
    true
}

fn taskbar_at_point(pt: POINT) -> Option<(usize, native_interop::TaskbarWindow)> {
    native_interop::find_taskbars()
        .into_iter()
        .enumerate()
        .find(|(_, taskbar)| {
            pt.x >= taskbar.rect.left
                && pt.x < taskbar.rect.right
                && pt.y >= taskbar.rect.top
                && pt.y < taskbar.rect.bottom
        })
}

fn tray_left_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT) -> i32 {
    let mut tray_left = taskbar_rect.right;
    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }
    tray_left
}

fn clamp_offset_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT, offset: i32) -> i32 {
    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
    let max_offset = (tray_left - taskbar_rect.left - total_widget_width()).max(0);
    offset.clamp(0, max_offset)
}

fn offset_for_drop_point(
    taskbar_hwnd: HWND,
    taskbar_rect: RECT,
    pt: POINT,
    drag_start_client_x: i32,
    taskbar_side: TaskbarSide,
) -> i32 {
    let desired_left = pt.x - taskbar_rect.left - drag_start_client_x;
    let offset = match taskbar_side {
        TaskbarSide::Right => {
            let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
            tray_left - taskbar_rect.left - total_widget_width() - desired_left
        }
        TaskbarSide::Left => desired_left,
    };
    clamp_offset_for_taskbar(taskbar_hwnd, taskbar_rect, offset)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session, strings);
        state.weekly_text = poller::format_line(&claude_code.weekly, strings);
    } else {
        state.session_text.clear();
        state.weekly_text.clear();
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_text =
            if codex.session.resets_at.is_none() && codex.session.percentage == 0.0 {
                String::new()
            } else {
                poller::format_line(&codex.session, strings)
            };
        state.codex_weekly_text =
            if codex.weekly.resets_at.is_none() && codex.weekly.percentage == 0.0 {
                String::new()
            } else {
                poller::format_line(&codex.weekly, strings)
            };
    } else {
        state.codex_session_text.clear();
        state.codex_weekly_text.clear();
    }

    if let Some(antigravity) = data.antigravity.as_ref() {
        state.antigravity_session_text = poller::format_line(&antigravity.session, strings);
        state.antigravity_weekly_text =
            if antigravity.weekly.resets_at.is_none() && antigravity.weekly.percentage == 0.0 {
                String::new()
            } else {
                poller::format_line(&antigravity.weekly, strings)
            };
    } else {
        state.antigravity_session_text.clear();
        state.antigravity_weekly_text.clear();
    }

    if let Some(grok) = data.grok.as_ref() {
        state.grok_session_text.clear();
        state.grok_weekly_text = poller::format_line(&grok.weekly, strings);
    } else {
        state.grok_session_text.clear();
        state.grok_weekly_text.clear();
    }

    if let Some(cursor) = data.cursor.as_ref() {
        state.cursor_session_text.clear();
        state.cursor_weekly_text = poller::format_line(&cursor.weekly, strings);
    } else {
        state.cursor_session_text.clear();
        state.cursor_weekly_text.clear();
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn resolve_cursor_agent_path() -> String {
    let preferred = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("cursor-agent").join("cursor-agent.cmd"));
    if let Some(path) = preferred.filter(|path| path.is_file()) {
        return path.to_string_lossy().into_owned();
    }

    "cursor-agent".to_string()
}

fn login_command(provider: LoginProvider) -> LoginCommand {
    match provider {
        LoginProvider::ClaudeCode => LoginCommand {
            executable: "claude".to_string(),
            args: vec![
                "auth".to_string(),
                "login".to_string(),
                "--claudeai".to_string(),
            ],
            provider_name: "Claude Code",
        },
        LoginProvider::Codex => LoginCommand {
            executable: "codex".to_string(),
            args: vec!["login".to_string()],
            provider_name: "Codex",
        },
        LoginProvider::Antigravity => LoginCommand {
            executable: "agy".to_string(),
            args: Vec::new(),
            provider_name: "Antigravity",
        },
        LoginProvider::Grok => LoginCommand {
            executable: "grok".to_string(),
            args: vec!["login".to_string()],
            provider_name: "Grok",
        },
        LoginProvider::Cursor => {
            let executable = resolve_cursor_agent_path();
            if executable.to_ascii_lowercase().ends_with(".cmd") {
                LoginCommand {
                    executable: "cmd.exe".to_string(),
                    args: vec!["/c".to_string(), executable, "login".to_string()],
                    provider_name: "Cursor",
                }
            } else {
                LoginCommand {
                    executable,
                    args: vec!["login".to_string()],
                    provider_name: "Cursor",
                }
            }
        }
    }
}

fn launch_login(hwnd: HWND, provider: LoginProvider) {
    let command_spec = login_command(provider);

    let mut command = Command::new(&command_spec.executable);
    command.args(&command_spec.args);
    command.creation_flags(CREATE_NEW_CONSOLE);
    if let Some(home) = dirs::home_dir() {
        command.current_dir(home);
    }

    match command.spawn() {
        Ok(_) => diagnose::log(format!(
            "launched {} login via {}",
            command_spec.provider_name, command_spec.executable
        )),
        Err(error) => {
            diagnose::log_error(
                &format!("unable to launch {}", command_spec.executable),
                error,
            );
            let strings = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.language.strings())
                    .unwrap_or(LanguageId::English.strings())
            };
            let (title, body) = match provider {
                LoginProvider::ClaudeCode => {
                    (strings.token_expired_title, strings.token_expired_body)
                }
                LoginProvider::Codex => (
                    strings.codex_token_expired_title,
                    strings.codex_token_expired_body,
                ),
                LoginProvider::Antigravity => (
                    strings.antigravity_token_expired_title,
                    strings.antigravity_token_expired_body,
                ),
                LoginProvider::Grok => (
                    strings.grok_token_expired_title,
                    strings.grok_token_expired_body,
                ),
                LoginProvider::Cursor => (
                    strings.cursor_token_expired_title,
                    strings.cursor_token_expired_body,
                ),
            };
            show_error_message(hwnd, title, body);
        }
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => match install_channel {
            InstallChannel::Portable => {
                format!(
                    "v{current} - {} v{}",
                    strings.update_to, release.latest_version
                )
            }
            InstallChannel::Winget => format!(
                "v{current} - {} v{}",
                localization::update_via_winget(language),
                release.latest_version
            ),
        },
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                show_info_message(
                    hwnd,
                    app_state.language.strings().updates,
                    app_state.language.strings().update_in_progress,
                );
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        (app_state.language.strings(), app_state.install_channel)
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    match install_channel {
                        InstallChannel::Portable => begin_update_apply(hwnd, release),
                        InstallChannel::Winget => begin_winget_update(hwnd),
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    match updater::begin_winget_update() {
        Ok(()) => unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        Err(error) => {
            let message = format!("{}.\n\n{}", strings.update_failed, error);
            show_error_message(hwnd, strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "ClaudeCodeUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const TEXT_WIDTH: i32 = 62;
const MODEL_RIGHT_MARGIN: i32 = 3;
const RIGHT_MARGIN: i32 = 1;
const WIDGET_HEIGHT: i32 = 46;

fn is_drag_handle_point(client_x: i32, client_y: i32) -> bool {
    let divider_h = sc(25);
    let divider_top = (sc(WIDGET_HEIGHT) - divider_h) / 2;
    client_x >= 0
        && client_x < sc(LEFT_DIVIDER_W)
        && client_y >= divider_top
        && client_y < divider_top + divider_h
}

fn cursor_is_on_drag_handle(hwnd: HWND) -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() || !ScreenToClient(hwnd, &mut pt).as_bool() {
            return false;
        }
        is_drag_handle_point(pt.x, pt.y)
    }
}

fn active_model_count(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
) -> i32 {
    (show_claude_code as i32
        + show_codex as i32
        + show_antigravity as i32
        + show_grok as i32
        + show_cursor as i32)
        .max(1)
}

fn toggle_provider_enabled(flags: &mut [bool; 5], index: usize) {
    let Some(enabled) = flags.get(index).copied() else {
        return;
    };
    if enabled && flags.iter().filter(|value| **value).count() == 1 {
        return;
    }
    flags[index] = !enabled;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelMenuEntry {
    id: u16,
    label: &'static str,
    checked: bool,
    provider: Option<Provider>,
}

fn model_menu_entries(
    strings: Strings,
    enabled: [bool; 5],
    auth_required: [bool; 5],
) -> Vec<ModelMenuEntry> {
    let providers = [
        (
            Provider::Claude,
            IDM_MODEL_CLAUDE_CODE,
            strings.claude_code_model,
            IDM_LOGIN_CLAUDE_CODE,
            strings.login_to_claude_code,
        ),
        (
            Provider::Codex,
            IDM_MODEL_CODEX,
            strings.codex_model,
            IDM_LOGIN_CODEX,
            strings.login_to_codex,
        ),
        (
            Provider::Antigravity,
            IDM_MODEL_ANTIGRAVITY,
            strings.antigravity_model,
            IDM_LOGIN_ANTIGRAVITY,
            strings.login_to_antigravity,
        ),
        (
            Provider::Grok,
            IDM_MODEL_GROK,
            strings.grok_model,
            IDM_LOGIN_GROK,
            strings.login_to_grok,
        ),
        (
            Provider::Cursor,
            IDM_MODEL_CURSOR,
            strings.cursor_model,
            IDM_LOGIN_CURSOR,
            strings.login_to_cursor,
        ),
    ];
    let mut entries = Vec::with_capacity(10);

    for (index, (provider, model_id, model_label, login_id, login_label)) in
        providers.into_iter().enumerate()
    {
        entries.push(ModelMenuEntry {
            id: model_id,
            label: model_label,
            checked: enabled[index],
            provider: Some(provider),
        });
        if enabled[index] && auth_required[index] {
            entries.push(ModelMenuEntry {
                id: login_id,
                label: login_label,
                checked: false,
                provider: None,
            });
        }
    }

    entries
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuItemVisualState {
    Normal,
    Selected,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuBackgroundColor {
    SystemMenu,
    SystemHighlight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuTextColor {
    Provider(Color),
    SystemColor(Color),
    SystemMenuText,
    SystemHighlightText,
    SystemGrayText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MenuItemColorDecision {
    background: MenuBackgroundColor,
    text: MenuTextColor,
}

fn menu_item_color_decision_for_background(
    provider: Provider,
    menu_background: Color,
    state: MenuItemVisualState,
    high_contrast: bool,
) -> MenuItemColorDecision {
    match state {
        MenuItemVisualState::Disabled => MenuItemColorDecision {
            background: MenuBackgroundColor::SystemMenu,
            text: MenuTextColor::SystemGrayText,
        },
        MenuItemVisualState::Selected => MenuItemColorDecision {
            background: MenuBackgroundColor::SystemHighlight,
            text: MenuTextColor::SystemHighlightText,
        },
        MenuItemVisualState::Normal if high_contrast => MenuItemColorDecision {
            background: MenuBackgroundColor::SystemMenu,
            text: MenuTextColor::SystemMenuText,
        },
        MenuItemVisualState::Normal => MenuItemColorDecision {
            background: MenuBackgroundColor::SystemMenu,
            text: provider_menu_text_color_for_background(provider, menu_background),
        },
    }
}

fn menu_item_background_color(
    background: MenuBackgroundColor,
    actual_menu_background: Color,
    system_menu_background: Color,
    system_highlight_background: Color,
    high_contrast: bool,
) -> Color {
    match background {
        MenuBackgroundColor::SystemMenu if high_contrast => system_menu_background,
        MenuBackgroundColor::SystemMenu => actual_menu_background,
        MenuBackgroundColor::SystemHighlight => system_highlight_background,
    }
}

fn srgb_luminance_component(component: u8) -> f64 {
    let component = component as f64 / 255.0;
    if component <= 0.03928 {
        component / 12.92
    } else {
        ((component + 0.055) / 1.055).powf(2.4)
    }
}

fn color_luminance(color: Color) -> f64 {
    0.2126 * srgb_luminance_component(color.r)
        + 0.7152 * srgb_luminance_component(color.g)
        + 0.0722 * srgb_luminance_component(color.b)
}

fn color_contrast_ratio(foreground: Color, background: Color) -> f64 {
    let foreground = color_luminance(foreground);
    let background = color_luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

const MIN_MENU_TEXT_CONTRAST: f64 = 4.5;

fn fallback_menu_text_color(background: Color) -> Color {
    let black = Color::new(0x00, 0x00, 0x00);
    let white = Color::new(0xFF, 0xFF, 0xFF);
    let black_contrast = color_contrast_ratio(black, background);
    let white_contrast = color_contrast_ratio(white, background);

    if black_contrast >= MIN_MENU_TEXT_CONTRAST && black_contrast >= white_contrast {
        black
    } else if white_contrast >= MIN_MENU_TEXT_CONTRAST {
        white
    } else {
        // The WCAG 4.5:1 black/white ranges overlap, so this branch is only
        // defensive. Keep the higher-contrast system extreme if rounding or
        // a future threshold change removes that overlap.
        if black_contrast >= white_contrast {
            black
        } else {
            white
        }
    }
}

fn provider_menu_text_color_for_background(provider: Provider, background: Color) -> MenuTextColor {
    let light_variant =
        native_interop::provider_color(provider, ProviderColorRole::MenuValue, false);
    let dark_variant = native_interop::provider_color(provider, ProviderColorRole::MenuValue, true);
    let (best_variant, best_contrast) = if color_contrast_ratio(light_variant, background)
        >= color_contrast_ratio(dark_variant, background)
    {
        (
            light_variant,
            color_contrast_ratio(light_variant, background),
        )
    } else {
        (dark_variant, color_contrast_ratio(dark_variant, background))
    };

    if best_contrast >= MIN_MENU_TEXT_CONTRAST {
        MenuTextColor::Provider(best_variant)
    } else {
        MenuTextColor::SystemColor(fallback_menu_text_color(background))
    }
}

fn menu_item_visual_state(item_state: u32) -> MenuItemVisualState {
    if item_state & (ODS_DISABLED.0 | ODS_GRAYED.0) != 0 {
        MenuItemVisualState::Disabled
    } else if item_state & (ODS_SELECTED.0 | ODS_HOTLIGHT.0) != 0 {
        MenuItemVisualState::Selected
    } else {
        MenuItemVisualState::Normal
    }
}

#[repr(C)]
struct OwnerDrawModelItem {
    // MSAAMENUINFO must be the first field. Windows accessibility clients
    // interpret dwItemData as this documented structure for owner-draw items.
    msaa: MSAAMENUINFO,
    id: u16,
    provider: Provider,
    is_dark: bool,
    label: Vec<u16>,
}

impl OwnerDrawModelItem {
    fn new(id: u16, provider: Provider, is_dark: bool, label: &str) -> Box<Self> {
        let label = native_interop::wide_str(label);
        let mut item = Box::new(Self {
            msaa: MSAAMENUINFO::default(),
            id,
            provider,
            is_dark,
            label,
        });
        let text_len = item.label.len().saturating_sub(1);
        item.msaa = MSAAMENUINFO {
            dwMSAASignature: MSAA_MENU_SIG as u32,
            cchWText: text_len as u32,
            pszWText: PWSTR::from_raw(item.label.as_mut_ptr()),
        };
        item
    }

    fn text(&self) -> &[u16] {
        let text_len = self.label.len().saturating_sub(1);
        &self.label[..text_len]
    }
}

#[derive(Default)]
struct OwnerDrawMenuStorage {
    items: Vec<Box<OwnerDrawModelItem>>,
    item_keys: Vec<(usize, u16)>,
}

thread_local! {
    /// Owner-draw item address/ID pairs are valid only while their popup
    /// storage is active. The registry is thread-local because native menu
    /// callbacks run synchronously on the window thread.
    static ACTIVE_OWNER_DRAW_ITEMS: RefCell<Vec<(usize, u16)>> = RefCell::new(Vec::new());
}

impl OwnerDrawMenuStorage {
    /// Append one owner-drawn provider row. The boxed item stays alive until
    /// show_context_menu returns, after TrackPopupMenu has stopped dispatching
    /// WM_MEASUREITEM/WM_DRAWITEM/WM_MENUCHAR for this menu.
    fn append_model_item(&mut self, menu: HMENU, entry: &ModelMenuEntry, is_dark: bool) -> bool {
        let Some(provider) = entry.provider else {
            return false;
        };

        let mut item = OwnerDrawModelItem::new(entry.id, provider, is_dark, entry.label);
        let item_data = (&*item) as *const OwnerDrawModelItem as usize;
        let text_len = item.text().len() as u32;
        let info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_STRING | MIIM_DATA,
            fType: MFT_OWNERDRAW,
            fState: if entry.checked {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: entry.id as u32,
            dwItemData: item_data,
            dwTypeData: PWSTR::from_raw(item.label.as_mut_ptr()),
            cch: text_len,
            ..Default::default()
        };

        let position = unsafe { GetMenuItemCount(menu) };
        if position < 0 {
            return false;
        }

        if unsafe { InsertMenuItemW(menu, position as u32, TRUE, &info) }.is_ok() {
            self.items.push(item);
            self.item_keys.push((item_data, entry.id));
            true
        } else {
            false
        }
    }

    fn activate(&self) -> OwnerDrawMenuActivation<'_> {
        ACTIVE_OWNER_DRAW_ITEMS.with(|active_items| {
            let mut active_items = active_items.borrow_mut();
            for item_key in &self.item_keys {
                if !active_items.contains(item_key) {
                    active_items.push(*item_key);
                }
            }
        });
        OwnerDrawMenuActivation { storage: self }
    }

    fn deactivate(&self) {
        if self.item_keys.is_empty() {
            return;
        }

        ACTIVE_OWNER_DRAW_ITEMS.with(|active_items| {
            let mut active_items = active_items.borrow_mut();
            active_items.retain(|key| !self.item_keys.contains(key));
        });
    }
}

struct OwnerDrawMenuActivation<'a> {
    storage: &'a OwnerDrawMenuStorage,
}

impl Drop for OwnerDrawMenuActivation<'_> {
    fn drop(&mut self) {
        self.storage.deactivate();
    }
}

impl Drop for OwnerDrawMenuStorage {
    fn drop(&mut self) {
        self.deactivate();
    }
}

fn provider_for_model_id(id: u16) -> Option<Provider> {
    match id {
        IDM_MODEL_CLAUDE_CODE => Some(Provider::Claude),
        IDM_MODEL_CODEX => Some(Provider::Codex),
        IDM_MODEL_ANTIGRAVITY => Some(Provider::Antigravity),
        IDM_MODEL_GROK => Some(Provider::Grok),
        IDM_MODEL_CURSOR => Some(Provider::Cursor),
        _ => None,
    }
}

fn is_active_owner_draw_item(item_data: usize, item_id: u16) -> bool {
    if item_data == 0 || provider_for_model_id(item_id).is_none() {
        return false;
    }

    ACTIVE_OWNER_DRAW_ITEMS
        .with(|active_items| active_items.borrow().contains(&(item_data, item_id)))
}

/// The item data comes from OwnerDrawMenuStorage, which remains in scope while
/// the native menu dispatches owner-draw callbacks. Validate both the command
/// ID and the scoped address before dereferencing the callback data.
unsafe fn owner_draw_model_item<'a>(
    item_data: usize,
    item_id: u16,
) -> Option<&'a OwnerDrawModelItem> {
    if !is_active_owner_draw_item(item_data, item_id) {
        return None;
    }
    let item = &*(item_data as *const OwnerDrawModelItem);
    if item.id != item_id || provider_for_model_id(item_id) != Some(item.provider) {
        return None;
    }
    Some(item)
}

fn high_contrast_enabled() -> bool {
    unsafe {
        let mut high_contrast = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            high_contrast.cbSize,
            Some(&mut high_contrast as *mut HIGHCONTRASTW as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && high_contrast.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0
    }
}

fn system_color(index: SYS_COLOR_INDEX) -> Color {
    let value = unsafe { GetSysColor(index) };
    Color::new(value as u8, (value >> 8) as u8, (value >> 16) as u8)
}

fn color_from_colorref(color: COLORREF) -> Color {
    Color::new(color.0 as u8, (color.0 >> 8) as u8, (color.0 >> 16) as u8)
}

fn resolve_menu_background_color(hwnd: HWND, hdc: HDC, rect: &RECT) -> Color {
    if hdc.is_invalid() {
        return system_color(COLOR_MENU);
    }

    unsafe {
        // Ask the active Windows menu theme first. COLOR_MENU reflects the
        // classic system color table and can disagree with a themed popup.
        let theme_class = native_interop::wide_str("Menu");
        let theme = OpenThemeData(hwnd, PCWSTR::from_raw(theme_class.as_ptr()));
        if !theme.is_invalid() {
            let themed_background =
                GetThemeColor(theme, MENU_POPUPBACKGROUND.0, 0, TMT_FILLCOLOR).ok();
            let _ = CloseThemeData(theme);
            if let Some(color) = themed_background {
                return color_from_colorref(color);
            }
        }

        // The owner-draw DC has already been prepared by the native menu
        // renderer. Read a corner pixel to retain the active theme's actual
        // surface, including Windows 10+ themed menu colors.
        let left = rect.left;
        let right = rect.right.saturating_sub(1).max(left);
        let top = rect.top;
        let bottom = rect.bottom.saturating_sub(1).max(top);
        let inset_left = left.saturating_add(2).min(right);
        let inset_right = right.saturating_sub(2).max(left);
        let inset_top = top.saturating_add(2).min(bottom);
        let inset_bottom = bottom.saturating_sub(2).max(top);
        for (x, y) in [
            (inset_left, inset_top),
            (inset_right, inset_top),
            (inset_right, inset_bottom),
            (inset_left, inset_bottom),
        ] {
            let pixel = GetPixel(hdc, x, y);
            if pixel.0 != CLR_INVALID {
                return color_from_colorref(pixel);
            }
        }

        // Some owner-draw DCs do not expose a readable pixel yet. Their
        // background color is still a better theme signal than the app's
        // registry-derived light/dark flag.
        let background = GetBkColor(hdc);
        if background.0 != CLR_INVALID {
            return color_from_colorref(background);
        }
    }

    system_color(COLOR_MENU)
}

fn menu_check_column_width() -> i32 {
    unsafe { GetSystemMetrics(SM_CXMENUCHECK).max(0) }
}

fn menu_horizontal_padding() -> i32 {
    unsafe { GetSystemMetrics(SM_CXEDGE).max(2) * 2 }
}

struct SystemMenuFont {
    object: HGDIOBJ,
    owned: Option<HFONT>,
}

impl SystemMenuFont {
    fn new() -> Self {
        unsafe {
            let mut metrics = NONCLIENTMETRICSW {
                cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            let owned = if SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                metrics.cbSize,
                Some(&mut metrics as *mut NONCLIENTMETRICSW as *mut std::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok()
            {
                let font = CreateFontIndirectW(&metrics.lfMenuFont);
                (!font.is_invalid()).then_some(font)
            } else {
                None
            };
            let object = owned
                .map(HGDIOBJ::from)
                .unwrap_or_else(|| GetStockObject(DEFAULT_GUI_FONT));
            Self { object, owned }
        }
    }

    unsafe fn select(&self, hdc: HDC) -> HGDIOBJ {
        SelectObject(hdc, self.object)
    }
}

impl Drop for SystemMenuFont {
    fn drop(&mut self) {
        if let Some(font) = self.owned {
            unsafe {
                let _ = DeleteObject(font);
            }
        }
    }
}

fn measure_owner_draw_model_item(measure_item: &mut MEASUREITEMSTRUCT) -> bool {
    if measure_item.CtlType.0 != ODT_MENU.0 {
        return false;
    }

    let item = unsafe { owner_draw_model_item(measure_item.itemData, measure_item.itemID as u16) };
    let Some(item) = item else {
        return false;
    };

    let text = item.text();
    let (text_width, text_height) = unsafe {
        let hdc = GetDC(HWND::default());
        if hdc.is_invalid() {
            (text.len() as i32 * 8, 16)
        } else {
            let menu_font = SystemMenuFont::new();
            let old_font = menu_font.select(hdc);
            let mut extent = SIZE::default();
            let measured = GetTextExtentPoint32W(hdc, text, &mut extent).as_bool();
            if !old_font.is_invalid() {
                SelectObject(hdc, old_font);
            }
            drop(menu_font);
            let _ = ReleaseDC(HWND::default(), hdc);
            if measured {
                (extent.cx, extent.cy)
            } else {
                (text.len() as i32 * 8, 16)
            }
        }
    };

    let system_menu_height = unsafe { GetSystemMetrics(SM_CYMENU).max(0) };
    let check_height = unsafe { GetSystemMetrics(SM_CYMENUCHECK).max(0) };
    measure_item.itemWidth =
        (text_width + menu_check_column_width() + menu_horizontal_padding()).max(1) as u32;
    measure_item.itemHeight = (text_height + 4)
        .max(system_menu_height)
        .max(check_height)
        .max(16) as u32;
    true
}

fn draw_owner_draw_model_item(hwnd: HWND, draw_item: &DRAWITEMSTRUCT) -> bool {
    if draw_item.CtlType.0 != ODT_MENU.0 {
        return false;
    }

    let item = unsafe { owner_draw_model_item(draw_item.itemData, draw_item.itemID as u16) };
    let Some(item) = item else {
        return false;
    };

    let state = menu_item_visual_state(draw_item.itemState.0);
    let high_contrast = high_contrast_enabled();
    let menu_background = resolve_menu_background_color(hwnd, draw_item.hDC, &draw_item.rcItem);
    let decision = menu_item_color_decision_for_background(
        item.provider,
        menu_background,
        state,
        high_contrast,
    );
    let background = menu_item_background_color(
        decision.background,
        menu_background,
        system_color(COLOR_MENU),
        system_color(COLOR_HIGHLIGHT),
        high_contrast,
    );
    let foreground = match decision.text {
        MenuTextColor::Provider(color) => color,
        MenuTextColor::SystemColor(color) => color,
        MenuTextColor::SystemMenuText => system_color(COLOR_MENUTEXT),
        MenuTextColor::SystemHighlightText => system_color(COLOR_HIGHLIGHTTEXT),
        MenuTextColor::SystemGrayText => system_color(COLOR_GRAYTEXT),
    };

    unsafe {
        // DrawTextW/DrawFrameControl change DC state. Keep the supplied menu
        // HDC isolated so native login rows drawn next to this item retain
        // their system font, background mode, and text color.
        let saved_dc = SaveDC(draw_item.hDC);
        if saved_dc == 0 {
            return false;
        }

        let menu_font = SystemMenuFont::new();
        let old_font = menu_font.select(draw_item.hDC);
        let mut item_rect = draw_item.rcItem;
        let brush = CreateSolidBrush(COLORREF(background.to_colorref()));
        let _ = FillRect(draw_item.hDC, &item_rect, brush);
        let _ = DeleteObject(brush);

        let check_width = menu_check_column_width();
        if draw_item.itemState.0 & ODS_CHECKED.0 != 0 && check_width > 0 {
            let check_height = GetSystemMetrics(SM_CYMENUCHECK).max(1);
            let mut check_rect = RECT {
                left: item_rect.left,
                top: item_rect.top + ((item_rect.bottom - item_rect.top - check_height) / 2),
                right: item_rect.left + check_width,
                bottom: item_rect.top
                    + ((item_rect.bottom - item_rect.top - check_height) / 2)
                    + check_height,
            };
            let check_state = if state == MenuItemVisualState::Disabled {
                DFCS_MENUCHECK | DFCS_INACTIVE
            } else {
                DFCS_MENUCHECK
            };
            let _ = DrawFrameControl(draw_item.hDC, &mut check_rect, DFC_MENU, check_state);
        }

        let _ = SetBkMode(draw_item.hDC, TRANSPARENT);
        let _ = SetTextColor(draw_item.hDC, COLORREF(foreground.to_colorref()));
        item_rect.left += check_width + menu_horizontal_padding() / 2;
        item_rect.right -= menu_horizontal_padding() / 2;
        let mut text = item.text().to_vec();
        let _ = DrawTextW(
            draw_item.hDC,
            &mut text,
            &mut item_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        if draw_item.itemState.0 & ODS_FOCUS.0 != 0
            && draw_item.itemState.0 & ODS_NOFOCUSRECT.0 == 0
        {
            let focus_rect = RECT {
                left: draw_item.rcItem.left + 1,
                top: draw_item.rcItem.top + 1,
                right: draw_item.rcItem.right - 1,
                bottom: draw_item.rcItem.bottom - 1,
            };
            let _ = DrawFocusRect(draw_item.hDC, &focus_rect);
        }

        if !old_font.is_invalid() {
            let _ = SelectObject(draw_item.hDC, old_font);
        }
        let _ = RestoreDC(draw_item.hDC, saved_dc);
        drop(menu_font);
    }
    true
}

fn menu_label_matches_char(text: &[u16], needle: u16) -> bool {
    fn same_char(left: u16, right: u16) -> bool {
        if left == right {
            return true;
        }
        char::from_u32(left as u32)
            .zip(char::from_u32(right as u32))
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
    }

    let mut first_char = None;
    let mut index = 0;
    while index < text.len() {
        let current = text[index];
        if current == b'&' as u16 {
            if index + 1 < text.len() {
                if text[index + 1] == b'&' as u16 {
                    first_char.get_or_insert(text[index + 1]);
                    index += 2;
                    continue;
                }
                return same_char(text[index + 1], needle);
            }
        } else {
            first_char.get_or_insert(current);
        }
        index += 1;
    }
    first_char.is_some_and(|first| same_char(first, needle))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelMenuCharMatch {
    position: u32,
    highlighted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelMenuCharAction {
    Execute(u32),
    Select(u32),
}

fn model_menu_char_action(matches: &[ModelMenuCharMatch]) -> Option<ModelMenuCharAction> {
    match matches {
        [] => None,
        [single] => Some(ModelMenuCharAction::Execute(single.position)),
        multiple => {
            let next_index = multiple
                .iter()
                .position(|item| item.highlighted)
                .map_or(0, |current| (current + 1) % multiple.len());
            Some(ModelMenuCharAction::Select(multiple[next_index].position))
        }
    }
}

fn encode_model_menu_char_result(action: ModelMenuCharAction) -> LRESULT {
    let (result_type, position) = match action {
        ModelMenuCharAction::Execute(position) => (MNC_EXECUTE, position),
        ModelMenuCharAction::Select(position) => (MNC_SELECT, position),
    };
    // WM_MENUCHAR's low word is the zero-based menu position. It is not the
    // command ID returned by MENUITEMINFO.wID.
    LRESULT(((result_type << 16) | (position & 0xFFFF)) as isize)
}

fn model_menu_char_result(menu: HMENU, menu_type: u32, needle: u16) -> Option<LRESULT> {
    // WM_MENUCHAR reports MF_POPUP for submenus and MF_SYSMENU for a system
    // menu. A zero type is also used for a regular popup menu.
    if menu_type != 0 && menu_type & (MF_POPUP.0 | MF_SYSMENU.0) == 0 {
        return None;
    }

    let count = unsafe { GetMenuItemCount(menu) };
    if count < 0 {
        return None;
    }

    let mut matches = Vec::new();
    for position in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_DATA,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(menu, position, TRUE, &mut info) }.is_err()
            || !info.fType.contains(MFT_OWNERDRAW)
        {
            continue;
        }

        let item = unsafe { owner_draw_model_item(info.dwItemData, info.wID as u16) };
        if item.is_some_and(|item| menu_label_matches_char(item.text(), needle)) {
            matches.push(ModelMenuCharMatch {
                position,
                highlighted: info.fState.contains(MFS_HILITE),
            });
        }
    }

    model_menu_char_action(&matches).map(encode_model_menu_char_result)
}

fn auth_watch_mode_for_failure(
    data: &AppUsageData,
    error: poller::PollError,
) -> Option<poller::CredentialWatchMode> {
    let auth_flags = [
        data.claude_code_auth_required,
        data.codex_auth_required,
        data.antigravity_auth_required,
        data.grok_auth_required,
        data.cursor_auth_required,
    ];
    let auth_count = auth_flags.into_iter().filter(|required| *required).count();

    if auth_count == 1 {
        if data.claude_code_auth_required {
            return Some(poller::CredentialWatchMode::ActiveSource);
        }
        if data.codex_auth_required {
            return Some(poller::CredentialWatchMode::Codex);
        }
        if data.antigravity_auth_required {
            return Some(poller::CredentialWatchMode::Antigravity);
        }
        if data.grok_auth_required {
            return Some(poller::CredentialWatchMode::Grok);
        }
        if data.cursor_auth_required {
            return Some(poller::CredentialWatchMode::Cursor);
        }
    }

    if auth_count > 1 {
        return Some(poller::CredentialWatchMode::AllSources);
    }

    match error {
        poller::PollError::AuthRequired | poller::PollError::TokenExpired => {
            Some(poller::CredentialWatchMode::ActiveSource)
        }
        poller::PollError::NoCredentials => Some(poller::CredentialWatchMode::AllSources),
        poller::PollError::RequestFailed => None,
    }
}

fn bottom_window_label(strings: Strings, show_cursor: bool) -> &'static str {
    if show_cursor {
        strings.weekly_monthly_window
    } else {
        strings.weekly_window
    }
}

fn top_window_label(
    strings: Strings,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> &'static str {
    if show_claude_code || show_codex || show_antigravity {
        strings.session_window
    } else {
        ""
    }
}

fn row_bar_segment_count(active_models: i32) -> i32 {
    match active_models {
        1 => SEGMENT_COUNT,
        2 => 5,
        _ => 4,
    }
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let bar_segments = row_bar_segment_count(active_models);
    let model_width = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * bar_segments - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH);

    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + sc(LABEL_WIDTH)
        + sc(LABEL_RIGHT_MARGIN)
        + model_width * active_models
        + sc(MODEL_RIGHT_MARGIN) * (active_models - 1)
        + sc(RIGHT_MARGIN)
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    total_widget_width_for(active_model_count(
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
        state.show_grok,
        state.show_cursor,
    ))
}

fn total_widget_width() -> i32 {
    let active_models = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| {
                active_model_count(
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                    s.show_cursor,
                )
            })
            .unwrap_or(1)
    };
    total_widget_width_for(active_models)
}

fn claude_accent_color() -> Color {
    native_interop::provider_color(Provider::Claude, ProviderColorRole::Bar, false)
}

fn codex_accent_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Codex, ProviderColorRole::Bar, is_dark)
}

fn antigravity_accent_color() -> Color {
    native_interop::provider_color(Provider::Antigravity, ProviderColorRole::Bar, false)
}

fn grok_accent_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Grok, ProviderColorRole::Bar, is_dark)
}

fn cursor_accent_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Cursor, ProviderColorRole::Bar, is_dark)
}

fn claude_usage_text_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Claude, ProviderColorRole::MenuValue, is_dark)
}

fn codex_usage_text_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Codex, ProviderColorRole::MenuValue, is_dark)
}

fn antigravity_usage_text_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Antigravity, ProviderColorRole::MenuValue, is_dark)
}

fn grok_usage_text_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Grok, ProviderColorRole::MenuValue, is_dark)
}

fn cursor_usage_text_color(is_dark: bool) -> Color {
    native_interop::provider_color(Provider::Cursor, ProviderColorRole::MenuValue, is_dark)
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running.
    // Exception: when relaunched after an explorer restart (ENV_RELAUNCH set),
    // wait for the previous instance to release the mutex, then take over.
    let is_relaunch = std::env::var(ENV_RELAUNCH).is_ok();
    let mutex_name = native_interop::wide_str("Global\\ClaudeCodeUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    if is_relaunch {
                        diagnose::log("relaunch: waiting for previous instance to exit");
                        let wait_result = WaitForSingleObject(h, 10_000);
                        if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
                            diagnose::log(format!(
                                "startup aborted: previous instance did not exit cleanly ({wait_result:?})"
                            ));
                            return;
                        }
                    } else {
                        diagnose::log("startup aborted: another instance is already running");
                        return;
                    }
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("ClaudeCodeUsageMonitor");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = load_settings();
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count = active_model_count(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
            settings.show_grok,
            settings.show_cursor,
        );
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(initial_model_count),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                session_percent: 0.0,
                session_text: String::new(),
                weekly_percent: 0.0,
                weekly_text: String::new(),
                codex_session_percent: 0.0,
                codex_session_text: String::new(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: String::new(),
                antigravity_session_percent: 0.0,
                antigravity_session_text: String::new(),
                antigravity_weekly_percent: 0.0,
                antigravity_weekly_text: String::new(),
                grok_session_percent: 0.0,
                grok_session_text: String::new(),
                grok_weekly_percent: 0.0,
                grok_weekly_text: String::new(),
                cursor_session_percent: 0.0,
                cursor_session_text: String::new(),
                cursor_weekly_percent: 0.0,
                cursor_weekly_text: String::new(),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
                show_grok: settings.show_grok,
                show_cursor: settings.show_cursor,
                claude_code_auth_required: false,
                codex_auth_required: false,
                antigravity_auth_required: false,
                grok_auth_required: false,
                cursor_auth_required: false,
                data: None,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                taskbar_index: settings.taskbar_index,
                taskbar_side: settings.taskbar_side,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_client_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
                shutdown_requested: false,
            });
        }

        // Try to embed in taskbar
        if attach_to_taskbar(hwnd, settings.taskbar_index) {
            embedded = true;
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Watch for explorer.exe restarts so we can re-embed and re-add the tray
        // icon (the shell discards tray registrations when it restarts). This
        // runs on a dedicated thread, NOT a window timer: once explorer destroys
        // the taskbar, our embedded child window stops receiving all messages
        // (WM_TIMER included), so a timer would never fire again.
        spawn_taskbar_watchdog();

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        embedded,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        grok_session_pct,
        grok_session_text,
        grok_weekly_pct,
        grok_weekly_text,
        cursor_session_pct,
        cursor_session_text,
        cursor_weekly_pct,
        cursor_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.embedded,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.grok_session_percent,
                s.grok_session_text.clone(),
                s.grok_weekly_percent,
                s.grok_weekly_text.clone(),
                s.cursor_session_percent,
                s.cursor_session_text.clone(),
                s.cursor_weekly_percent,
                s.cursor_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
                s.show_grok,
                s.show_cursor,
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let antigravity_accent = antigravity_accent_color();
    let grok_accent = grok_accent_color(is_dark);
    let cursor_accent = cursor_accent_color(is_dark);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            grok_session_pct,
            &grok_session_text,
            grok_weekly_pct,
            &grok_weekly_text,
            cursor_session_pct,
            &cursor_session_text,
            cursor_weekly_pct,
            &cursor_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            show_cursor,
            &codex_accent,
            &antigravity_accent,
            &grok_accent,
            &cursor_accent,
        );

        // Background pixels → alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    accent: &Color,
    track: &Color,
    strings: Strings,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    antigravity_session_pct: f64,
    antigravity_session_text: &str,
    antigravity_weekly_pct: f64,
    antigravity_weekly_text: &str,
    grok_session_pct: f64,
    grok_session_text: &str,
    grok_weekly_pct: f64,
    grok_weekly_text: &str,
    cursor_session_pct: f64,
    cursor_session_text: &str,
    cursor_weekly_pct: f64,
    cursor_weekly_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
    codex_accent: &Color,
    antigravity_accent: &Color,
    grok_accent: &Color,
    cursor_accent: &Color,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_bottom = divider_top + divider_h;

        let (div_left, div_right) = if is_dark {
            ((80, 80, 80), (40, 40, 40))
        } else {
            ((160, 160, 160), (230, 230, 230))
        };

        let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_left.0, div_left.1, div_left.2,
        )));
        let left_rect = RECT {
            left: 0,
            top: divider_top,
            right: sc(2),
            bottom: divider_bottom,
        };
        FillRect(hdc, &left_rect, left_brush);
        let _ = DeleteObject(left_brush);

        let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_right.0,
            div_right.1,
            div_right.2,
        )));
        let right_rect = RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        };
        FillRect(hdc, &right_rect, right_brush);
        let _ = DeleteObject(right_brush);

        let content_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
        let row2_y = height - sc(5) - sc(SEGMENT_H);
        let row1_y = row2_y - sc(10) - sc(SEGMENT_H);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        draw_row(
            hdc,
            content_x,
            row1_y,
            is_dark,
            text_color,
            top_window_label(strings, show_claude_code, show_codex, show_antigravity),
            true,
            session_pct,
            session_text,
            codex_session_pct,
            codex_session_text,
            antigravity_session_pct,
            antigravity_session_text,
            grok_session_pct,
            grok_session_text,
            cursor_session_pct,
            cursor_session_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            show_cursor,
            accent,
            codex_accent,
            antigravity_accent,
            grok_accent,
            cursor_accent,
            track,
        );
        draw_row(
            hdc,
            content_x,
            row2_y,
            is_dark,
            text_color,
            bottom_window_label(strings, show_cursor),
            false,
            weekly_pct,
            weekly_text,
            codex_weekly_pct,
            codex_weekly_text,
            antigravity_weekly_pct,
            antigravity_weekly_text,
            grok_weekly_pct,
            grok_weekly_text,
            cursor_weekly_pct,
            cursor_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            show_cursor,
            accent,
            codex_accent,
            antigravity_accent,
            grok_accent,
            cursor_accent,
            track,
        );

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let (show_claude_code, show_codex, show_antigravity, show_grok, show_cursor) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| {
                (
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                    s.show_cursor,
                )
            })
            .unwrap_or((true, false, false, false, false))
    };

    match poller::poll(
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
    ) {
        Ok(data) => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if let Some(claude_code) = data.claude_code.as_ref() {
                    s.session_percent = claude_code.session.percentage;
                    s.weekly_percent = claude_code.weekly.percentage;
                } else if s.show_claude_code {
                    s.session_percent = 0.0;
                    s.weekly_percent = 0.0;
                }
                if let Some(codex) = data.codex.as_ref() {
                    s.codex_session_percent = codex.session.percentage;
                    s.codex_weekly_percent = codex.weekly.percentage;
                } else if s.show_codex {
                    s.codex_session_percent = 0.0;
                    s.codex_weekly_percent = 0.0;
                }
                if let Some(antigravity) = data.antigravity.as_ref() {
                    s.antigravity_session_percent = antigravity.session.percentage;
                    s.antigravity_weekly_percent = antigravity.weekly.percentage;
                } else if s.show_antigravity {
                    s.antigravity_session_percent = 0.0;
                    s.antigravity_weekly_percent = 0.0;
                }
                if let Some(grok) = data.grok.as_ref() {
                    s.grok_session_percent = 0.0;
                    s.grok_weekly_percent = grok.weekly.percentage;
                } else if s.show_grok {
                    s.grok_session_percent = 0.0;
                    s.grok_weekly_percent = 0.0;
                }
                if let Some(cursor) = data.cursor.as_ref() {
                    s.cursor_session_percent = 0.0;
                    s.cursor_weekly_percent = cursor.weekly.percentage;
                } else if s.show_cursor {
                    s.cursor_session_percent = 0.0;
                    s.cursor_weekly_percent = 0.0;
                }
                s.claude_code_auth_required = data.claude_code_auth_required;
                s.codex_auth_required = data.codex_auth_required;
                s.antigravity_auth_required = data.antigravity_auth_required;
                s.grok_auth_required = data.grok_auth_required;
                s.cursor_auth_required = data.cursor_auth_required;
                // Stop fast-poll if reset data is now fresh
                if !poller::app_is_past_reset(&data) {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(data);
                s.last_poll_ok = true;
                refresh_usage_texts(s);

                // Recovered from errors — restore normal poll interval
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
                    }
                }
                s.force_notify_auth_error = false;
                s.auth_error_paused_polling = false;
                s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                s.auth_watch_snapshot.clear();
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(failure) => {
            let e = failure.error;
            let auth_watch = auth_watch_mode_for_failure(&failure.data, e)
                .map(|watch_mode| (watch_mode, poller::credential_watch_snapshot(watch_mode)));
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    s.claude_code_auth_required = failure.data.claude_code_auth_required;
                    s.codex_auth_required = failure.data.codex_auth_required;
                    s.antigravity_auth_required = failure.data.antigravity_auth_required;
                    s.grok_auth_required = failure.data.grok_auth_required;
                    s.cursor_auth_required = failure.data.cursor_auth_required;
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            s.session_text.clear();
                            s.weekly_text.clear();
                            s.codex_session_text.clear();
                            s.codex_weekly_text.clear();
                            s.antigravity_session_text.clear();
                            s.antigravity_weekly_text.clear();
                            s.grok_session_text.clear();
                            s.grok_weekly_text.clear();
                            s.cursor_session_text.clear();
                            s.cursor_weekly_text.clear();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            // Transient network / credential-missing errors: exponential backoff.
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            s.session_text.clear();
                            s.weekly_text.clear();
                            s.codex_session_text.clear();
                            s.codex_weekly_text.clear();
                            s.antigravity_session_text.clear();
                            s.antigravity_weekly_text.clear();
                            s.grok_session_text.clear();
                            s.grok_weekly_text.clear();
                            s.cursor_session_text.clear();
                            s.cursor_weekly_text.clear();
                            s.retry_count = s.retry_count.saturating_add(1);
                            let backoff = RETRY_BASE_MS.saturating_mul(
                                1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                            );
                            let retry_ms = backoff.min(s.poll_interval_ms);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        if s.show_claude_code && s.claude_code_auth_required {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Claude,
                                s.language.strings().token_expired_title,
                                s.language.strings().token_expired_body,
                            )
                        } else if s.show_codex && s.codex_auth_required {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Codex,
                                s.language.strings().codex_token_expired_title,
                                s.language.strings().codex_token_expired_body,
                            )
                        } else if s.show_antigravity && s.antigravity_auth_required {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Antigravity,
                                s.language.strings().antigravity_token_expired_title,
                                s.language.strings().antigravity_token_expired_body,
                            )
                        } else if s.show_grok && s.grok_auth_required {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Grok,
                                s.language.strings().grok_token_expired_title,
                                s.language.strings().grok_token_expired_body,
                            )
                        } else if s.show_cursor && s.cursor_auth_required {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Cursor,
                                s.language.strings().cursor_token_expired_title,
                                s.language.strings().cursor_token_expired_body,
                            )
                        } else if s.show_claude_code {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Claude,
                                s.language.strings().token_expired_title,
                                s.language.strings().token_expired_body,
                            )
                        } else if s.show_codex {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Codex,
                                s.language.strings().codex_token_expired_title,
                                s.language.strings().codex_token_expired_body,
                            )
                        } else if s.show_antigravity {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Antigravity,
                                s.language.strings().antigravity_token_expired_title,
                                s.language.strings().antigravity_token_expired_body,
                            )
                        } else if s.show_grok {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Grok,
                                s.language.strings().grok_token_expired_title,
                                s.language.strings().grok_token_expired_body,
                            )
                        } else {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Cursor,
                                s.language.strings().cursor_token_expired_title,
                                s.language.strings().cursor_token_expired_body,
                            )
                        }
                    })
                };
                if let Some((_strings, kind, title, body)) = balloon {
                    tray_icon::notify_balloon(hwnd, kind, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::app_is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.grok
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.cursor
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, tray_offset, taskbar_hwnd, taskbar_side) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            s.tray_offset,
            taskbar_hwnd,
            s.taskbar_side,
        )
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();
    let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
    let tray_offset = tray_offset.clamp(0, max_offset);
    let offset_changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.tray_offset != tray_offset {
                s.tray_offset = tray_offset;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if offset_changed {
        save_state_settings();
    }

    let widget_height = sc(WIDGET_HEIGHT);
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = match taskbar_side {
            TaskbarSide::Right => tray_left - taskbar_rect.left - widget_width - tray_offset,
            TaskbarSide::Left => tray_offset,
        };
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    } else {
        // Topmost popup: screen coordinates
        let x = match taskbar_side {
            TaskbarSide::Right => tray_left - widget_width - tray_offset,
            TaskbarSide::Left => taskbar_rect.left + tray_offset,
        };
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MEASUREITEM => {
            if lparam.0 != 0
                && measure_owner_draw_model_item(&mut *(lparam.0 as *mut MEASUREITEMSTRUCT))
            {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_DRAWITEM => {
            if lparam.0 != 0
                && draw_owner_draw_model_item(hwnd, &*(lparam.0 as *const DRAWITEMSTRUCT))
            {
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_MENUCHAR => {
            let menu = HMENU(lparam.0 as *mut _);
            let menu_type = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let character = (wparam.0 & 0xFFFF) as u16;
            model_menu_char_result(menu, menu_type, character)
                .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            schedule_countdown_timer();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            sync_tray_icons(hwnd);
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if cursor_is_on_drag_handle(hwnd) {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if !is_drag_handle_point(client_x, client_y) {
                return LRESULT(0);
            }

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.dragging = true;
                s.drag_start_mouse_x = pt.x;
                s.drag_start_client_x = client_x;
                s.drag_start_offset = s.tray_offset;
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Positive delta = further from the anchor side: moving the
                    // mouse left when right-anchored, right when left-anchored.
                    let delta = match s.taskbar_side {
                        TaskbarSide::Right => s.drag_start_mouse_x - pt.x,
                        TaskbarSide::Left => pt.x - s.drag_start_mouse_x,
                    };
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go past the anchor side)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width_for_state(s);
                            let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = match s.taskbar_side {
                                TaskbarSide::Right => {
                                    if embedded {
                                        tray_left - taskbar_rect.left - widget_width - new_offset
                                    } else {
                                        tray_left - widget_width - new_offset
                                    }
                                }
                                TaskbarSide::Left => {
                                    if embedded {
                                        new_offset
                                    } else {
                                        taskbar_rect.left + new_offset
                                    }
                                }
                            };
                            Some((
                                hwnd_val,
                                embedded,
                                x,
                                y,
                                taskbar_rect.top,
                                widget_width,
                                widget_height,
                            ))
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let drag_result = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        Some((s.taskbar_index, s.drag_start_client_x, s.taskbar_side))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((current_taskbar_index, drag_start_client_x, taskbar_side)) = drag_result {
                let _ = ReleaseCapture();
                if let Some((target_index, target_taskbar)) = taskbar_at_point(pt) {
                    if target_index != current_taskbar_index {
                        let new_offset = offset_for_drop_point(
                            target_taskbar.hwnd,
                            target_taskbar.rect,
                            pt,
                            drag_start_client_x,
                            taskbar_side,
                        );
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.tray_offset = new_offset;
                            }
                        }
                        if attach_to_taskbar(hwnd, target_index) {
                            position_at_taskbar();
                            render_layered();
                        }
                    }
                }
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.grok_session_text.clear();
                            s.grok_weekly_text = "...".to_string();
                            s.cursor_session_text.clear();
                            s.cursor_weekly_text = "...".to_string();
                            s.force_notify_auth_error = true;
                        }
                    }
                    render_layered();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_SIDE_LEFT | IDM_SIDE_RIGHT => {
                    let new_side = if id == IDM_SIDE_LEFT {
                        TaskbarSide::Left
                    } else {
                        TaskbarSide::Right
                    };
                    let changed = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            if s.taskbar_side != new_side {
                                s.taskbar_side = new_side;
                                s.tray_offset = 0;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    };
                    if changed {
                        save_state_settings();
                        position_at_taskbar();
                        render_layered();
                    }
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                IDM_LOGIN_CLAUDE_CODE => launch_login(hwnd, LoginProvider::ClaudeCode),
                IDM_LOGIN_CODEX => launch_login(hwnd, LoginProvider::Codex),
                IDM_LOGIN_ANTIGRAVITY => launch_login(hwnd, LoginProvider::Antigravity),
                IDM_LOGIN_GROK => launch_login(hwnd, LoginProvider::Grok),
                IDM_LOGIN_CURSOR => launch_login(hwnd, LoginProvider::Cursor),
                IDM_MODEL_CLAUDE_CODE
                | IDM_MODEL_CODEX
                | IDM_MODEL_ANTIGRAVITY
                | IDM_MODEL_GROK
                | IDM_MODEL_CURSOR => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            let mut flags = [
                                s.show_claude_code,
                                s.show_codex,
                                s.show_antigravity,
                                s.show_grok,
                                s.show_cursor,
                            ];
                            let index = match id {
                                IDM_MODEL_CLAUDE_CODE => 0,
                                IDM_MODEL_CODEX => 1,
                                IDM_MODEL_ANTIGRAVITY => 2,
                                IDM_MODEL_GROK => 3,
                                IDM_MODEL_CURSOR => 4,
                                _ => return LRESULT(0),
                            };
                            toggle_provider_enabled(&mut flags, index);
                            [
                                &mut s.show_claude_code,
                                &mut s.show_codex,
                                &mut s.show_antigravity,
                                &mut s.show_grok,
                                &mut s.show_cursor,
                            ]
                            .into_iter()
                            .zip(flags)
                            .for_each(|(target, value)| *target = value);
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.antigravity_session_text = "...".to_string();
                            s.antigravity_weekly_text = "...".to_string();
                            s.grok_session_text.clear();
                            s.grok_weekly_text = "...".to_string();
                            s.cursor_session_text.clear();
                            s.cursor_weekly_text = "...".to_string();
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_TRADITIONAL_CHINESE
                | IDM_LANG_SIMPLIFIED_CHINESE
                | IDM_LANG_RUSSIAN
                | IDM_LANG_PORTUGUESE_BRAZIL => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        IDM_LANG_SIMPLIFIED_CHINESE => Some(LanguageId::SimplifiedChinese),
                        IDM_LANG_RUSSIAN => Some(LanguageId::Russian),
                        IDM_LANG_PORTUGUESE_BRAZIL => Some(LanguageId::PortugueseBrazil),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                id if (IDM_MONITOR_BASE..=IDM_MONITOR_MAX).contains(&id) => {
                    let target_index = (id - IDM_MONITOR_BASE) as usize;
                    let current_index = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.taskbar_index)
                    };
                    if current_index.is_some_and(|current| current != target_index)
                        && attach_to_taskbar(hwnd, target_index)
                    {
                        save_state_settings();
                        position_at_taskbar();
                        render_layered();
                    }
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_widget_visibility(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.shutdown_requested = true;
            }
            drop(state);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            let (hook, wait_for_taskbar_recovery) = {
                let state = lock_state();
                match state.as_ref() {
                    Some(s) => (
                        s.win_event_hook,
                        should_wait_for_taskbar_recovery(s.embedded, s.shutdown_requested),
                    ),
                    None => (None, false),
                }
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            if wait_for_taskbar_recovery {
                diagnose::log(
                    "embedded window destroyed with taskbar; waiting for watchdog recovery",
                );
                return LRESULT(0);
            }
            tray_icon::remove_all(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auth_watch_mode_for_failure, bottom_window_label, encode_model_menu_char_result,
        enforce_provider_invariant, is_active_owner_draw_item, login_command,
        menu_item_background_color, menu_item_color_decision_for_background,
        menu_item_visual_state, menu_label_matches_char, model_menu_char_action,
        model_menu_entries, provider_for_model_id, provider_menu_text_color_for_background,
        should_wait_for_taskbar_recovery, taskbar_recovery_needed, toggle_provider_enabled,
        top_window_label, usage_slot_draw_states, LoginProvider, MenuBackgroundColor,
        MenuItemVisualState, MenuTextColor, ModelMenuCharAction, ModelMenuCharMatch,
        OwnerDrawMenuStorage, SettingsFile, IDM_LOGIN_ANTIGRAVITY, IDM_LOGIN_CLAUDE_CODE,
        IDM_LOGIN_CODEX, IDM_LOGIN_CURSOR, IDM_LOGIN_GROK, IDM_MODEL_ANTIGRAVITY,
        IDM_MODEL_CLAUDE_CODE, IDM_MODEL_CODEX, IDM_MODEL_CURSOR, IDM_MODEL_GROK, IDM_MONITOR_BASE,
        IDM_MONITOR_MAX, MNC_EXECUTE, MNC_SELECT, ODS_DISABLED, ODS_HOTLIGHT, ODS_SELECTED,
    };
    use crate::models::AppUsageData;
    use crate::native_interop::{self, Color, Provider, ProviderColorRole};
    use crate::poller::{CredentialWatchMode, PollError};
    use crate::tray_icon;

    #[test]
    fn provider_login_commands_do_not_collide_with_widget_toggle() {
        assert_ne!(IDM_LOGIN_CLAUDE_CODE, tray_icon::IDM_TOGGLE_WIDGET);
        assert_ne!(IDM_LOGIN_CODEX, tray_icon::IDM_TOGGLE_WIDGET);
        assert_ne!(IDM_LOGIN_ANTIGRAVITY, tray_icon::IDM_TOGGLE_WIDGET);
        assert_ne!(IDM_LOGIN_GROK, tray_icon::IDM_TOGGLE_WIDGET);
        assert_ne!(IDM_LOGIN_CURSOR, tray_icon::IDM_TOGGLE_WIDGET);
        for id in [
            IDM_MODEL_CLAUDE_CODE,
            IDM_MODEL_CODEX,
            IDM_MODEL_ANTIGRAVITY,
            IDM_MODEL_GROK,
            IDM_MODEL_CURSOR,
            IDM_LOGIN_CLAUDE_CODE,
            IDM_LOGIN_CODEX,
            IDM_LOGIN_ANTIGRAVITY,
            IDM_LOGIN_GROK,
            IDM_LOGIN_CURSOR,
        ] {
            assert_ne!(id, tray_icon::IDM_TOGGLE_WIDGET);
            assert!(!(IDM_MONITOR_BASE..=IDM_MONITOR_MAX).contains(&id));
        }
    }

    #[test]
    fn provider_login_commands_match_installed_clis() {
        assert_eq!(
            login_command(LoginProvider::ClaudeCode).executable,
            "claude"
        );
        assert_eq!(
            login_command(LoginProvider::ClaudeCode).args,
            ["auth", "login", "--claudeai"]
        );
        assert_eq!(login_command(LoginProvider::Codex).executable, "codex");
        assert_eq!(login_command(LoginProvider::Codex).args, ["login"]);
        assert_eq!(login_command(LoginProvider::Antigravity).executable, "agy");
        assert!(login_command(LoginProvider::Antigravity).args.is_empty());
        assert_eq!(login_command(LoginProvider::Grok).executable, "grok");
        assert_eq!(login_command(LoginProvider::Grok).args, ["login"]);
        let cursor = login_command(LoginProvider::Cursor);
        assert_eq!(cursor.args.last().map(String::as_str), Some("login"));
        assert!(cursor.executable == "cursor-agent" || cursor.executable == "cmd.exe");
        if cursor.executable == "cmd.exe" {
            assert_eq!(cursor.args.first().map(String::as_str), Some("/c"));
        }
    }

    #[test]
    fn only_unexpected_embedded_destruction_waits_for_recovery() {
        assert!(should_wait_for_taskbar_recovery(true, false));
        assert!(!should_wait_for_taskbar_recovery(true, true));
        assert!(!should_wait_for_taskbar_recovery(false, false));
    }

    #[test]
    fn missing_widget_or_taskbar_triggers_recovery() {
        assert!(!taskbar_recovery_needed(true, true));
        assert!(taskbar_recovery_needed(false, true));
        assert!(taskbar_recovery_needed(true, false));
    }

    #[test]
    fn at_least_one_provider_stays_enabled() {
        let mut flags = [true, false, false, false, false];
        toggle_provider_enabled(&mut flags, 0);
        assert_eq!(flags, [true, false, false, false, false]);

        toggle_provider_enabled(&mut flags, 4);
        assert_eq!(flags, [true, false, false, false, true]);
        toggle_provider_enabled(&mut flags, 0);
        assert_eq!(flags, [false, false, false, false, true]);
        toggle_provider_enabled(&mut flags, 4);
        assert_eq!(flags, [false, false, false, false, true]);
    }

    #[test]
    fn auth_required_provider_can_be_disabled_while_another_remains_enabled() {
        let mut flags = [false, false, false, true, true];
        let entries = model_menu_entries(
            crate::localization::LanguageId::English.strings(),
            flags,
            [false, false, false, true, false],
        );

        assert_eq!(entries[3].id, IDM_MODEL_GROK);
        assert_eq!(entries[3].label, "Grok");
        toggle_provider_enabled(&mut flags, 3);

        assert_eq!(flags, [false, false, false, false, true]);
    }

    #[test]
    fn model_login_action_is_adjacent_only_for_enabled_auth_required_provider() {
        let entries = model_menu_entries(
            crate::localization::LanguageId::English.strings(),
            [true, false, false, true, false],
            [false, false, true, true, true],
        );
        let ids: Vec<u16> = entries.iter().map(|entry| entry.id).collect();

        assert_eq!(
            ids,
            vec![
                IDM_MODEL_CLAUDE_CODE,
                IDM_MODEL_CODEX,
                IDM_MODEL_ANTIGRAVITY,
                IDM_MODEL_GROK,
                IDM_LOGIN_GROK,
                IDM_MODEL_CURSOR,
            ]
        );
        assert_eq!(entries[3].label, "Grok");
        assert_eq!(entries[4].label, "Log in to Grok...");
        assert_eq!(entries[3].provider, Some(Provider::Grok));
        assert_eq!(entries[4].provider, None);
    }

    #[test]
    fn model_rows_keep_provider_metadata_and_native_login_rows() {
        let entries = model_menu_entries(
            crate::localization::LanguageId::English.strings(),
            [true, true, true, true, true],
            [true, false, true, false, true],
        );
        let expected_providers = [
            Provider::Claude,
            Provider::Codex,
            Provider::Antigravity,
            Provider::Grok,
            Provider::Cursor,
        ];
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![
                IDM_MODEL_CLAUDE_CODE,
                IDM_LOGIN_CLAUDE_CODE,
                IDM_MODEL_CODEX,
                IDM_MODEL_ANTIGRAVITY,
                IDM_LOGIN_ANTIGRAVITY,
                IDM_MODEL_GROK,
                IDM_MODEL_CURSOR,
                IDM_LOGIN_CURSOR,
            ]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.checked)
                .collect::<Vec<_>>(),
            vec![true, false, true, true, false, true, true, false]
        );

        for provider in expected_providers {
            let model = entries
                .iter()
                .find(|entry| entry.provider == Some(provider))
                .expect("every provider row has metadata");
            assert_eq!(
                native_interop::provider_color(provider, ProviderColorRole::MenuValue, false),
                match provider {
                    Provider::Claude => Color::new(0xA9, 0x4F, 0x32),
                    Provider::Codex => Color::new(0x1F, 0x1F, 0x1F),
                    Provider::Antigravity => Color::new(0x19, 0x67, 0xD2),
                    Provider::Grok => Color::new(0x07, 0x5E, 0x54),
                    Provider::Cursor => Color::new(0x6D, 0x28, 0xD9),
                }
            );
            assert_eq!(provider_for_model_id(model.id), Some(provider));
        }

        assert!(entries
            .iter()
            .filter(|entry| entry.provider.is_none())
            .all(|entry| entry.id >= IDM_LOGIN_CLAUDE_CODE && !entry.checked));
    }

    #[test]
    fn model_menu_colors_use_provider_only_for_normal_non_high_contrast_rows() {
        let light_surface = Color::new(0xFA, 0xFA, 0xFA);
        let dark_surface = Color::new(0x20, 0x20, 0x20);
        let normal = menu_item_color_decision_for_background(
            Provider::Grok,
            light_surface,
            MenuItemVisualState::Normal,
            false,
        );
        assert_eq!(normal.background, MenuBackgroundColor::SystemMenu);
        assert_eq!(
            normal.text,
            MenuTextColor::Provider(Color::new(0x07, 0x5E, 0x54))
        );

        let dark = menu_item_color_decision_for_background(
            Provider::Grok,
            dark_surface,
            MenuItemVisualState::Normal,
            false,
        );
        assert_eq!(
            dark.text,
            MenuTextColor::Provider(Color::new(0x14, 0xB8, 0xA6))
        );
        assert_eq!(
            menu_item_color_decision_for_background(
                Provider::Grok,
                dark_surface,
                MenuItemVisualState::Selected,
                false,
            )
            .text,
            MenuTextColor::SystemHighlightText
        );
        assert_eq!(
            menu_item_color_decision_for_background(
                Provider::Grok,
                dark_surface,
                MenuItemVisualState::Disabled,
                false,
            )
            .text,
            MenuTextColor::SystemGrayText
        );
        assert_eq!(
            menu_item_color_decision_for_background(
                Provider::Grok,
                dark_surface,
                MenuItemVisualState::Normal,
                true,
            )
            .text,
            MenuTextColor::SystemMenuText
        );
    }

    #[test]
    fn model_menu_colors_choose_the_variant_that_contrasts_with_actual_surface() {
        let light_surface = Color::new(0xFA, 0xFA, 0xFA);
        let dark_surface = Color::new(0x20, 0x20, 0x20);

        assert_eq!(
            menu_item_color_decision_for_background(
                Provider::Grok,
                light_surface,
                MenuItemVisualState::Normal,
                false,
            )
            .text,
            MenuTextColor::Provider(Color::new(0x07, 0x5E, 0x54))
        );
        assert_eq!(
            menu_item_color_decision_for_background(
                Provider::Grok,
                dark_surface,
                MenuItemVisualState::Normal,
                false,
            )
            .text,
            MenuTextColor::Provider(Color::new(0x14, 0xB8, 0xA6))
        );
    }

    #[test]
    fn provider_menu_colors_keep_a_4_5_to_1_floor_on_midtone_surfaces() {
        let providers = [
            Provider::Claude,
            Provider::Codex,
            Provider::Antigravity,
            Provider::Grok,
            Provider::Cursor,
        ];
        let backgrounds = [
            Color::new(0x66, 0x66, 0x66),
            Color::new(0x77, 0x77, 0x77),
            Color::new(0x88, 0x88, 0x88),
        ];

        for provider in providers {
            for background in backgrounds {
                let text = match provider_menu_text_color_for_background(provider, background) {
                    MenuTextColor::Provider(color) | MenuTextColor::SystemColor(color) => color,
                    other => panic!("unexpected system menu state: {other:?}"),
                };
                assert!(
                    super::color_contrast_ratio(text, background) >= 4.5,
                    "{provider:?} text {text:?} on {background:?}"
                );
            }
        }

        let midtone = Color::new(0x77, 0x77, 0x77);
        assert_eq!(
            provider_menu_text_color_for_background(Provider::Claude, midtone),
            MenuTextColor::SystemColor(Color::new(0x00, 0x00, 0x00))
        );
        assert_eq!(
            provider_menu_text_color_for_background(Provider::Codex, midtone),
            MenuTextColor::SystemColor(Color::new(0x00, 0x00, 0x00))
        );
        assert_eq!(
            provider_menu_text_color_for_background(Provider::Antigravity, midtone),
            MenuTextColor::SystemColor(Color::new(0x00, 0x00, 0x00))
        );
        assert_eq!(
            provider_menu_text_color_for_background(Provider::Grok, midtone),
            MenuTextColor::SystemColor(Color::new(0x00, 0x00, 0x00))
        );
        assert_eq!(
            provider_menu_text_color_for_background(Provider::Cursor, midtone),
            MenuTextColor::SystemColor(Color::new(0x00, 0x00, 0x00))
        );
    }

    #[test]
    fn high_contrast_menu_background_uses_system_pair() {
        let actual_menu = Color::new(0x12, 0x34, 0x56);
        let system_menu = Color::new(0x01, 0x02, 0x03);
        let system_highlight = Color::new(0xA0, 0xB0, 0xC0);
        let normal = menu_item_color_decision_for_background(
            Provider::Grok,
            actual_menu,
            MenuItemVisualState::Normal,
            true,
        );
        assert_eq!(normal.text, MenuTextColor::SystemMenuText);
        assert_eq!(
            menu_item_background_color(
                normal.background,
                actual_menu,
                system_menu,
                system_highlight,
                true,
            ),
            system_menu
        );
        assert_eq!(
            menu_item_background_color(
                normal.background,
                actual_menu,
                system_menu,
                system_highlight,
                false,
            ),
            actual_menu
        );

        let selected = menu_item_color_decision_for_background(
            Provider::Grok,
            actual_menu,
            MenuItemVisualState::Selected,
            true,
        );
        assert_eq!(selected.text, MenuTextColor::SystemHighlightText);
        assert_eq!(
            menu_item_background_color(
                selected.background,
                actual_menu,
                system_menu,
                system_highlight,
                true,
            ),
            system_highlight
        );
    }

    #[test]
    fn model_menu_char_unique_result_uses_zero_based_position() {
        let result = encode_model_menu_char_result(ModelMenuCharAction::Execute(4));
        assert_eq!((result.0 as u32) >> 16, MNC_EXECUTE);
        assert_eq!(result.0 as u32 & 0xFFFF, 4);
    }

    #[test]
    fn model_menu_char_duplicate_matches_cycle_from_highlighted_row() {
        let matches = [
            ModelMenuCharMatch {
                position: 1,
                highlighted: false,
            },
            ModelMenuCharMatch {
                position: 3,
                highlighted: true,
            },
            ModelMenuCharMatch {
                position: 5,
                highlighted: false,
            },
        ];
        assert_eq!(
            model_menu_char_action(&matches),
            Some(ModelMenuCharAction::Select(5))
        );

        let wrapped = [
            ModelMenuCharMatch {
                position: 1,
                highlighted: false,
            },
            ModelMenuCharMatch {
                position: 3,
                highlighted: false,
            },
            ModelMenuCharMatch {
                position: 5,
                highlighted: true,
            },
        ];
        assert_eq!(
            model_menu_char_action(&wrapped),
            Some(ModelMenuCharAction::Select(1))
        );
        assert_eq!(
            encode_model_menu_char_result(ModelMenuCharAction::Select(5)).0 as u32 >> 16,
            MNC_SELECT
        );
    }

    #[test]
    fn owner_draw_visual_state_prioritizes_disabled_then_selection_or_hot() {
        assert_eq!(menu_item_visual_state(0), MenuItemVisualState::Normal);
        assert_eq!(
            menu_item_visual_state(ODS_SELECTED.0),
            MenuItemVisualState::Selected
        );
        assert_eq!(
            menu_item_visual_state(ODS_HOTLIGHT.0),
            MenuItemVisualState::Selected
        );
        assert_eq!(
            menu_item_visual_state(ODS_SELECTED.0 | ODS_DISABLED.0),
            MenuItemVisualState::Disabled
        );
    }

    #[test]
    fn owner_draw_registry_rejects_foreign_or_non_model_item_data() {
        assert!(!is_active_owner_draw_item(
            usize::MAX,
            IDM_MODEL_CLAUDE_CODE
        ));
        assert!(!is_active_owner_draw_item(0, IDM_MODEL_CLAUDE_CODE));
        assert!(!is_active_owner_draw_item(
            usize::MAX,
            IDM_LOGIN_CLAUDE_CODE
        ));
    }

    #[test]
    fn owner_draw_registry_requires_exact_item_id_pair() {
        super::ACTIVE_OWNER_DRAW_ITEMS.with(|active_items| {
            active_items.borrow_mut().clear();
            active_items
                .borrow_mut()
                .push((0x1234, IDM_MODEL_CLAUDE_CODE));
        });
        assert!(is_active_owner_draw_item(0x1234, IDM_MODEL_CLAUDE_CODE));
        assert!(!is_active_owner_draw_item(0x1234, IDM_MODEL_CODEX));
        super::ACTIVE_OWNER_DRAW_ITEMS.with(|active_items| active_items.borrow_mut().clear());
    }

    #[test]
    fn owner_draw_activation_guard_clears_registry_on_drop() {
        let mut storage = OwnerDrawMenuStorage::default();
        storage.item_keys.push((0x1234, IDM_MODEL_CLAUDE_CODE));
        {
            let _activation = storage.activate();
            assert!(is_active_owner_draw_item(0x1234, IDM_MODEL_CLAUDE_CODE));
        }
        assert!(!is_active_owner_draw_item(0x1234, IDM_MODEL_CLAUDE_CODE));
    }

    #[test]
    fn owner_draw_menu_labels_keep_native_mnemonic_shape() {
        assert!(menu_label_matches_char(
            &native_interop::wide_str("&Grok"),
            b'g' as u16
        ));
        assert!(menu_label_matches_char(
            &native_interop::wide_str("Cursor"),
            b'c' as u16
        ));
        assert!(!menu_label_matches_char(
            &native_interop::wide_str("&Grok"),
            b'c' as u16
        ));
    }

    #[test]
    fn cursor_uses_mixed_bottom_label_and_new_only_providers_have_no_top_label() {
        let strings = crate::localization::LanguageId::English.strings();
        assert_eq!(bottom_window_label(strings, false), "7d");
        assert_eq!(bottom_window_label(strings, true), "7d/mo");
        assert_eq!(top_window_label(strings, false, false, false), "");
        assert_eq!(top_window_label(strings, true, false, false), "5h");
    }

    #[test]
    fn new_provider_top_cells_never_draw_any_content() {
        let top_slots = usage_slot_draw_states(
            true,
            false,
            false,
            false,
            true,
            true,
            "",
            "",
            "",
            "stale 42%",
            "...",
        );
        assert_eq!(top_slots, [false, false, false, false, false]);

        let bottom_slots = usage_slot_draw_states(
            false,
            false,
            false,
            false,
            true,
            true,
            "",
            "",
            "",
            "37% · 2d",
            "12% · 9d",
        );
        assert_eq!(bottom_slots, [false, false, false, true, true]);
    }

    #[test]
    fn old_settings_default_new_providers_off_and_empty_selection_recovers() {
        let mut settings: SettingsFile = serde_json::from_str(
            r#"{
                "show_claude_code": false,
                "show_codex": false,
                "show_antigravity": false
            }"#,
        )
        .unwrap();
        assert!(!settings.show_grok);
        assert!(!settings.show_cursor);
        enforce_provider_invariant(&mut settings);
        assert!(settings.show_claude_code);
    }

    #[test]
    fn all_provider_auth_failure_watches_every_credential_source() {
        let data = AppUsageData {
            claude_code_auth_required: true,
            codex_auth_required: true,
            antigravity_auth_required: true,
            grok_auth_required: true,
            cursor_auth_required: true,
            ..AppUsageData::default()
        };

        assert_eq!(
            auth_watch_mode_for_failure(&data, PollError::AuthRequired),
            Some(CredentialWatchMode::AllSources)
        );

        let snapshot = crate::poller::credential_watch_snapshot(CredentialWatchMode::AllSources);
        assert!(snapshot
            .iter()
            .any(|entry| entry.starts_with("win:") || entry.starts_with("wsl:")));
        assert!(snapshot.iter().any(|entry| entry.starts_with("codex|")));
        assert!(snapshot
            .iter()
            .any(|entry| entry.starts_with("gemini:antigravity|")));
        assert!(snapshot.iter().any(|entry| entry.starts_with("grok|")));
        assert!(snapshot.iter().any(|entry| entry.starts_with("cursor|")));
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            is_dark,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            show_cursor,
            claude_code_auth_required,
            codex_auth_required,
            antigravity_auth_required,
            grok_auth_required,
            cursor_auth_required,
            taskbar_side,
            taskbar_index,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.is_dark,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                    s.show_cursor,
                    s.claude_code_auth_required,
                    s.codex_auth_required,
                    s.antigravity_auth_required,
                    s.grok_auth_required,
                    s.cursor_auth_required,
                    s.taskbar_side,
                    s.taskbar_index,
                ),
                None => (
                    POLL_15_MIN,
                    false,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    true,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    TaskbarSide::default(),
                    0,
                ),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Update Frequency submenu
        let freq_menu = CreatePopupMenu().unwrap();
        let freq_items: [(u16, u32, &str); 4] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Models submenu
        let models_menu = CreatePopupMenu().unwrap();
        let mut owner_draw_storage = OwnerDrawMenuStorage::default();
        let model_entries = model_menu_entries(
            strings,
            [
                show_claude_code,
                show_codex,
                show_antigravity,
                show_grok,
                show_cursor,
            ],
            [
                claude_code_auth_required,
                codex_auth_required,
                antigravity_auth_required,
                grok_auth_required,
                cursor_auth_required,
            ],
        );
        for entry in model_entries {
            if entry.provider.is_some() {
                let _ = owner_draw_storage.append_model_item(models_menu, &entry, is_dark);
            } else {
                let label = native_interop::wide_str(entry.label);
                let flags = if entry.checked {
                    MF_CHECKED
                } else {
                    MENU_ITEM_FLAGS(0)
                };
                let _ = AppendMenuW(
                    models_menu,
                    flags,
                    entry.id as usize,
                    PCWSTR::from_raw(label.as_ptr()),
                );
            }
        }

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let authentication_menu = CreatePopupMenu().unwrap();
        let authentication_items = [
            (IDM_LOGIN_CLAUDE_CODE, strings.relogin_to_claude_code),
            (IDM_LOGIN_CODEX, strings.relogin_to_codex),
            (IDM_LOGIN_ANTIGRAVITY, strings.relogin_to_antigravity),
            (IDM_LOGIN_GROK, strings.relogin_to_grok),
            (IDM_LOGIN_CURSOR, strings.relogin_to_cursor),
        ];
        for (id, label) in authentication_items {
            let label_str = native_interop::wide_str(label);
            let _ = AppendMenuW(
                authentication_menu,
                MENU_ITEM_FLAGS(0),
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }
        let authentication_label = native_interop::wide_str(strings.authentication);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            authentication_menu.0 as usize,
            PCWSTR::from_raw(authentication_label.as_ptr()),
        );

        // Taskbar side submenu
        let side_menu = CreatePopupMenu().unwrap();
        let side_items: [(u16, TaskbarSide, &str); 2] = [
            (IDM_SIDE_LEFT, TaskbarSide::Left, strings.taskbar_side_left),
            (
                IDM_SIDE_RIGHT,
                TaskbarSide::Right,
                strings.taskbar_side_right,
            ),
        ];
        for (id, side, label) in side_items {
            let label_str = native_interop::wide_str(label);
            let flags = if side == taskbar_side {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                side_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let side_label = native_interop::wide_str(strings.taskbar_side);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            side_menu.0 as usize,
            PCWSTR::from_raw(side_label.as_ptr()),
        );

        // Monitor submenu: one entry per detected taskbar (only shown when
        // there is more than one)
        let taskbars = native_interop::find_taskbars();
        if taskbars.len() > 1 {
            let monitor_menu = CreatePopupMenu().unwrap();
            let max_items = (IDM_MONITOR_MAX - IDM_MONITOR_BASE + 1) as usize;
            for (index, taskbar) in taskbars.iter().take(max_items).enumerate() {
                let number = native_interop::monitor_number_for_rect(taskbar.rect)
                    .unwrap_or(index as u32 + 1);
                let label = format!("{} {}", strings.monitor, number);
                let label_str = native_interop::wide_str(&label);
                let flags = if index == taskbar_index {
                    MF_CHECKED
                } else {
                    MENU_ITEM_FLAGS(0)
                };
                let _ = AppendMenuW(
                    monitor_menu,
                    flags,
                    IDM_MONITOR_BASE as usize + index,
                    PCWSTR::from_raw(label_str.as_ptr()),
                );
            }

            let monitor_label = native_interop::wide_str(strings.monitor);
            let _ = AppendMenuW(
                settings_menu,
                MF_POPUP,
                monitor_menu.0 as usize,
                PCWSTR::from_raw(monitor_label.as_ptr()),
            );
        }

        let language_menu = CreatePopupMenu().unwrap();
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
                LanguageId::SimplifiedChinese => IDM_LANG_SIMPLIFIED_CHINESE,
                LanguageId::Russian => IDM_LANG_RUSSIAN,
                LanguageId::PortugueseBrazil => IDM_LANG_PORTUGUESE_BRAZIL,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if matches!(
            update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        {
            // WM_MEASUREITEM/WM_DRAWITEM/WM_MENUCHAR are synchronous. Keep
            // callback keys active only for TrackPopupMenu and clear them on
            // every return or unwind through the guard's Drop.
            let _owner_draw_activation = owner_draw_storage.activate();
            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        }
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        grok_session_pct,
        grok_session_text,
        grok_weekly_pct,
        grok_weekly_text,
        cursor_session_pct,
        cursor_session_text,
        cursor_weekly_pct,
        cursor_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.grok_session_percent,
                s.grok_session_text.clone(),
                s.grok_weekly_percent,
                s.grok_weekly_text.clone(),
                s.cursor_session_percent,
                s.cursor_session_text.clone(),
                s.cursor_weekly_percent,
                s.cursor_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
                s.show_grok,
                s.show_cursor,
            ),
            None => return,
        }
    };

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let antigravity_accent = antigravity_accent_color();
    let grok_accent = grok_accent_color(is_dark);
    let cursor_accent = cursor_accent_color(is_dark);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            grok_session_pct,
            &grok_session_text,
            grok_weekly_pct,
            &grok_weekly_text,
            cursor_session_pct,
            &cursor_session_text,
            cursor_weekly_pct,
            &cursor_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            show_cursor,
            &codex_accent,
            &antigravity_accent,
            &grok_accent,
            &cursor_accent,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_row(
    hdc: HDC,
    x: i32,
    y: i32,
    is_dark: bool,
    text_color: &Color,
    label: &str,
    top_row: bool,
    claude_percent: f64,
    claude_text: &str,
    codex_percent: f64,
    codex_text: &str,
    antigravity_percent: f64,
    antigravity_text: &str,
    grok_percent: f64,
    grok_text: &str,
    cursor_percent: f64,
    cursor_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
    claude_accent: &Color,
    codex_accent: &Color,
    antigravity_accent: &Color,
    grok_accent: &Color,
    cursor_accent: &Color,
    track: &Color,
) {
    let seg_h = sc(SEGMENT_H);
    let active_models = active_model_count(
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
    );
    let segment_count = row_bar_segment_count(active_models);
    let claude_value_color = claude_usage_text_color(is_dark);
    let codex_value_color = codex_usage_text_color(is_dark);
    let antigravity_value_color = antigravity_usage_text_color(is_dark);
    let grok_value_color = grok_usage_text_color(is_dark);
    let cursor_value_color = cursor_usage_text_color(is_dark);
    let draw_slots = usage_slot_draw_states(
        top_row,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
        show_cursor,
        claude_text,
        codex_text,
        antigravity_text,
        grok_text,
        cursor_text,
    );

    unsafe {
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let mut model_x = x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN);
        if show_claude_code {
            if draw_slots[0] {
                draw_usage_bar(
                    hdc,
                    model_x,
                    y,
                    segment_count,
                    claude_percent,
                    claude_text,
                    claude_accent,
                    track,
                    &claude_value_color,
                );
            }
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_codex {
            if draw_slots[1] {
                draw_usage_bar(
                    hdc,
                    model_x,
                    y,
                    segment_count,
                    codex_percent,
                    codex_text,
                    codex_accent,
                    track,
                    &codex_value_color,
                );
            }
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_antigravity {
            if draw_slots[2] {
                draw_usage_bar(
                    hdc,
                    model_x,
                    y,
                    segment_count,
                    antigravity_percent,
                    antigravity_text,
                    antigravity_accent,
                    track,
                    &antigravity_value_color,
                );
            }
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_grok {
            if draw_slots[3] {
                draw_usage_bar(
                    hdc,
                    model_x,
                    y,
                    segment_count,
                    grok_percent,
                    grok_text,
                    grok_accent,
                    track,
                    &grok_value_color,
                );
            }
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if draw_slots[4] {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                cursor_percent,
                cursor_text,
                cursor_accent,
                track,
                &cursor_value_color,
            );
        }
    }
}

fn usage_slot_draw_states(
    top_row: bool,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    show_cursor: bool,
    claude_text: &str,
    codex_text: &str,
    antigravity_text: &str,
    grok_text: &str,
    cursor_text: &str,
) -> [bool; 5] {
    [
        show_claude_code && !claude_text.is_empty(),
        show_codex && !codex_text.is_empty(),
        show_antigravity && !antigravity_text.is_empty(),
        show_grok && !top_row && !grok_text.is_empty(),
        show_cursor && !top_row && !cursor_text.is_empty(),
    ]
}

fn model_usage_width(segment_count: i32) -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * segment_count - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH)
}

fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    text_color: &Color,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let segment_percent = 100.0 / segment_count as f64;

        for i in 0..segment_count {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_percent;
            let seg_end = seg_start + segment_percent;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let text_x = bar_x + segment_count * (seg_w + seg_gap) - seg_gap + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}
