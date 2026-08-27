# v1.7.1

This is the first release of the Kuu44 fork of Claude Code Usage Monitor. It is based on the upstream Code Zeno v1.4.9 release and includes the accumulated fork changes documented in [CHANGELOG.md](https://github.com/Kuu44/Claude-Code-Usage-Monitor/blob/v1.7.1/CHANGELOG.md).

## Highlights

- Left/right taskbar-side placement and direct multi-monitor selection.
- Correct handling for providers that do not expose a 5-hour or weekly usage window.
- Crash-resistant taskbar recovery when Windows switches between extended and single-monitor modes.
- Provider login and force-relogin actions for Claude Code, Codex, Antigravity, Grok, and Cursor.
- Grok Build weekly credits and Cursor monthly plan usage.
- Consistent provider colors in usage bars, usage text, tray badges, and the Models menu.

## Install and verify

Download the [Windows executable](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/download/v1.7.1/claude-code-usage-monitor.exe) and [SHA256SUMS.txt](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/download/v1.7.1/SHA256SUMS.txt). Verify the executable's SHA-256 hash before running it. The README contains a copy-and-paste PowerShell verification command.

The CodeZeno WinGet package remains the upstream distribution channel. This fork does not submit its builds to WinGet; use the direct release assets above for the Kuu44 build.
