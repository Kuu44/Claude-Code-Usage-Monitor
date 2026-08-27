use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug)]
pub struct TaskbarWindow {
    pub hwnd: HWND,
    pub rect: RECT,
}

pub fn find_taskbars() -> Vec<TaskbarWindow> {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let taskbars = &mut *(lparam.0 as *mut Vec<TaskbarWindow>);
        let mut class_name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_TrayWnd" || class_name == "Shell_SecondaryTrayWnd" {
                if let Some(rect) = get_taskbar_rect(hwnd).or_else(|| get_window_rect_safe(hwnd)) {
                    taskbars.push(TaskbarWindow { hwnd, rect });
                }
            }
        }
        BOOL(1)
    }

    let mut taskbars: Vec<TaskbarWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut taskbars as *mut _ as isize));
    }
    taskbars.sort_by_key(|taskbar| {
        (
            taskbar.rect.top,
            taskbar.rect.left,
            taskbar.rect.bottom,
            taskbar.rect.right,
        )
    });
    taskbars
}

/// Windows display device number (the `n` in `\\.\DISPLAYn`, matching the
/// numbering shown in display settings for typical setups) of the monitor
/// containing the given rect.
pub fn monitor_number_for_rect(rect: RECT) -> Option<u32> {
    unsafe {
        let center = POINT {
            x: rect.left + (rect.right - rect.left) / 2,
            y: rect.top + (rect.bottom - rect.top) / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(
            monitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
        .as_bool()
        {
            return None;
        }
        let device_len = info
            .szDevice
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(info.szDevice.len());
        let device = String::from_utf16_lossy(&info.szDevice[..device_len]);
        device
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|digits| digits.parse::<u32>().ok())
    }
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut class_name = [0u16; 64];
        let len = GetClassNameW(taskbar_hwnd, &mut class_name);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_SecondaryTrayWnd" {
                return get_window_rect_safe(taskbar_hwnd);
            }
        }

        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

pub fn window_exists(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd).as_bool() }
}

/// Embed our window as a child of the taskbar
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) {
    unsafe {
        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[allow(dead_code)]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Self { r, g, b }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}

/// The monitor's provider identities. These colors are semantic UI colors,
/// not claims about official provider branding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Provider {
    Claude,
    Codex,
    Antigravity,
    Grok,
    Cursor,
}

/// A palette role shared by the widget, usage values, menus, and tray badges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderColorRole {
    Bar,
    MenuValue,
    TrayFill,
    TrayText,
    TrayHighUsageFill,
    TrayHighUsageText,
}

/// Resolve a provider color for a role and theme.
///
/// `Tray*` roles intentionally ignore `is_dark`: tray badges are theme
/// agnostic, while widget bars and menu/value text need theme-aware contrast.
pub fn provider_color(provider: Provider, role: ProviderColorRole, is_dark: bool) -> Color {
    match (provider, role) {
        (Provider::Claude, ProviderColorRole::Bar) => Color::new(0xD9, 0x77, 0x57),
        (Provider::Claude, ProviderColorRole::MenuValue) if is_dark => Color::new(0xF0, 0x9A, 0x7A),
        (Provider::Claude, ProviderColorRole::MenuValue) => Color::new(0xA9, 0x4F, 0x32),
        (Provider::Claude, ProviderColorRole::TrayFill) => Color::new(0xD9, 0x77, 0x57),
        (Provider::Claude, ProviderColorRole::TrayText) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Claude, ProviderColorRole::TrayHighUsageFill) => Color::new(0xD9, 0x77, 0x57),
        (Provider::Claude, ProviderColorRole::TrayHighUsageText) => Color::new(0xFF, 0xFF, 0xFF),

        (Provider::Codex, ProviderColorRole::Bar)
        | (Provider::Codex, ProviderColorRole::MenuValue)
            if is_dark =>
        {
            Color::new(0xF5, 0xF5, 0xF5)
        }
        (Provider::Codex, ProviderColorRole::Bar)
        | (Provider::Codex, ProviderColorRole::MenuValue) => Color::new(0x1F, 0x1F, 0x1F),
        (Provider::Codex, ProviderColorRole::TrayFill) => Color::new(0x1F, 0x1F, 0x1F),
        (Provider::Codex, ProviderColorRole::TrayText) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Codex, ProviderColorRole::TrayHighUsageFill) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Codex, ProviderColorRole::TrayHighUsageText) => Color::new(0x1F, 0x1F, 0x1F),

        (Provider::Antigravity, ProviderColorRole::Bar) => Color::new(0x42, 0x85, 0xF4),
        (Provider::Antigravity, ProviderColorRole::MenuValue) if is_dark => {
            Color::new(0x8A, 0xB4, 0xF8)
        }
        (Provider::Antigravity, ProviderColorRole::MenuValue) => Color::new(0x19, 0x67, 0xD2),
        (Provider::Antigravity, ProviderColorRole::TrayFill) => Color::new(0x42, 0x85, 0xF4),
        (Provider::Antigravity, ProviderColorRole::TrayText) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Antigravity, ProviderColorRole::TrayHighUsageFill) => {
            Color::new(0xFF, 0xFF, 0xFF)
        }
        (Provider::Antigravity, ProviderColorRole::TrayHighUsageText) => {
            Color::new(0x19, 0x67, 0xD2)
        }

        (Provider::Grok, ProviderColorRole::Bar)
        | (Provider::Grok, ProviderColorRole::MenuValue)
            if is_dark =>
        {
            Color::new(0x14, 0xB8, 0xA6)
        }
        (Provider::Grok, ProviderColorRole::Bar)
        | (Provider::Grok, ProviderColorRole::MenuValue) => Color::new(0x07, 0x5E, 0x54),
        (Provider::Grok, ProviderColorRole::TrayFill) => Color::new(0x07, 0x5E, 0x54),
        (Provider::Grok, ProviderColorRole::TrayText) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Grok, ProviderColorRole::TrayHighUsageFill) => Color::new(0xCC, 0xFB, 0xF1),
        (Provider::Grok, ProviderColorRole::TrayHighUsageText) => Color::new(0x07, 0x5E, 0x54),

        (Provider::Cursor, ProviderColorRole::Bar)
        | (Provider::Cursor, ProviderColorRole::MenuValue)
            if is_dark =>
        {
            Color::new(0xC4, 0xB5, 0xFD)
        }
        (Provider::Cursor, ProviderColorRole::Bar)
        | (Provider::Cursor, ProviderColorRole::MenuValue) => Color::new(0x6D, 0x28, 0xD9),
        (Provider::Cursor, ProviderColorRole::TrayFill) => Color::new(0x6D, 0x28, 0xD9),
        (Provider::Cursor, ProviderColorRole::TrayText) => Color::new(0xFF, 0xFF, 0xFF),
        (Provider::Cursor, ProviderColorRole::TrayHighUsageFill) => Color::new(0xED, 0xE9, 0xFE),
        (Provider::Cursor, ProviderColorRole::TrayHighUsageText) => Color::new(0x6D, 0x28, 0xD9),
    }
}

#[cfg(test)]
mod tests {
    use super::{provider_color, Color, Provider, ProviderColorRole};

    #[test]
    fn provider_palette_matches_monitor_semantics() {
        let expected = [
            (
                Provider::Claude,
                Color::new(0xD9, 0x77, 0x57),
                Color::new(0xD9, 0x77, 0x57),
                Color::new(0xA9, 0x4F, 0x32),
                Color::new(0xF0, 0x9A, 0x7A),
            ),
            (
                Provider::Codex,
                Color::new(0x1F, 0x1F, 0x1F),
                Color::new(0xF5, 0xF5, 0xF5),
                Color::new(0x1F, 0x1F, 0x1F),
                Color::new(0xF5, 0xF5, 0xF5),
            ),
            (
                Provider::Antigravity,
                Color::new(0x42, 0x85, 0xF4),
                Color::new(0x42, 0x85, 0xF4),
                Color::new(0x19, 0x67, 0xD2),
                Color::new(0x8A, 0xB4, 0xF8),
            ),
            (
                Provider::Grok,
                Color::new(0x07, 0x5E, 0x54),
                Color::new(0x14, 0xB8, 0xA6),
                Color::new(0x07, 0x5E, 0x54),
                Color::new(0x14, 0xB8, 0xA6),
            ),
            (
                Provider::Cursor,
                Color::new(0x6D, 0x28, 0xD9),
                Color::new(0xC4, 0xB5, 0xFD),
                Color::new(0x6D, 0x28, 0xD9),
                Color::new(0xC4, 0xB5, 0xFD),
            ),
        ];

        for (provider, light_bar, dark_bar, light_menu, dark_menu) in expected {
            assert_eq!(
                provider_color(provider, ProviderColorRole::Bar, false),
                light_bar
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::Bar, true),
                dark_bar
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::MenuValue, false),
                light_menu
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::MenuValue, true),
                dark_menu
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayText, false),
                Color::new(0xFF, 0xFF, 0xFF)
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayText, true),
                Color::new(0xFF, 0xFF, 0xFF)
            );
        }
    }

    #[test]
    fn tray_palette_keeps_theme_agnostic_provider_and_inverse_states() {
        let expected = [
            (
                Provider::Claude,
                Color::new(0xD9, 0x77, 0x57),
                Color::new(0xD9, 0x77, 0x57),
                Color::new(0xFF, 0xFF, 0xFF),
            ),
            (
                Provider::Codex,
                Color::new(0x1F, 0x1F, 0x1F),
                Color::new(0xFF, 0xFF, 0xFF),
                Color::new(0x1F, 0x1F, 0x1F),
            ),
            (
                Provider::Antigravity,
                Color::new(0x42, 0x85, 0xF4),
                Color::new(0xFF, 0xFF, 0xFF),
                Color::new(0x19, 0x67, 0xD2),
            ),
            (
                Provider::Grok,
                Color::new(0x07, 0x5E, 0x54),
                Color::new(0xCC, 0xFB, 0xF1),
                Color::new(0x07, 0x5E, 0x54),
            ),
            (
                Provider::Cursor,
                Color::new(0x6D, 0x28, 0xD9),
                Color::new(0xED, 0xE9, 0xFE),
                Color::new(0x6D, 0x28, 0xD9),
            ),
        ];

        for (provider, fill, inverse_fill, inverse_text) in expected {
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayFill, false),
                fill
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayFill, true),
                fill
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayHighUsageFill, false),
                inverse_fill
            );
            assert_eq!(
                provider_color(provider, ProviderColorRole::TrayHighUsageText, false),
                inverse_text
            );
        }
    }
}
