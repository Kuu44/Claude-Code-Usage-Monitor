![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Latest fork release](https://img.shields.io/github/v/release/Kuu44/Claude-Code-Usage-Monitor?label=latest%20fork%20release)](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/latest)
[![Release workflow](https://github.com/Kuu44/Claude-Code-Usage-Monitor/actions/workflows/release.yml/badge.svg)](https://github.com/Kuu44/Claude-Code-Usage-Monitor/actions/workflows/release.yml)

# Claude Code Usage Monitor

![Screenshot](.github/animation.gif)

A lightweight Windows taskbar widget for people already using Claude Code, with optional Codex, Google Antigravity, Grok Build, and Cursor usage display.

It sits in your taskbar and shows how much of your Claude Code, Codex, Antigravity, Grok Build, and/or Cursor usage window you have left, without needing to open the terminal or the provider site.

This fork's current release is [v1.7.1](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/tag/v1.7.1).

## What You Get

- A **5h** bar for your current 5-hour Claude usage window
- A **7d** bar for your current 7-day window
- Optional Codex usage bars alongside Claude Code
- Optional Antigravity model usage bars for Google's 5-hour and weekly Gemini quota windows
- Optional Grok Build weekly credit usage
- Optional Cursor monthly plan usage
- Provider-specific login and force-relogin actions in **Settings → Authentication**
- A live countdown until each limit resets
- A small native widget that lives directly in the Windows taskbar
- System tray icon badges showing your enabled model usage percentage
- Left-click the tray icon to toggle the taskbar widget on or off
- Right-click options for refresh, displayed models, update frequency, language, startup, widget visibility, and updates
- Multi-monitor taskbar placement, so the widget can live on the taskbar for the screen you prefer
- Clear provider identity through matching bar, usage-text, menu, and tray colors: Claude warm orange, Codex monochrome, Antigravity blue, Grok teal, and Cursor purple

## Who This Is For

This app is for Windows users who already have **Claude Code (CLI or App) installed and signed in**.

Codex support is optional. To show Codex usage, install and sign in to the Codex CLI, then enable Codex from the right-click **Models** menu.

Antigravity support is optional too. To show Antigravity usage, install Google Antigravity, run `agy` in a terminal and sign in there, then enable the **Antigravity** model from the right-click **Models** menu.

Grok Build support is optional. To show Grok's weekly credits, install Grok Build, run `grok login`, then enable **Grok** from the right-click **Models** menu.

Cursor support is optional. To show Cursor's monthly plan usage, install Cursor Agent, run `cursor-agent login`, then enable **Cursor** from the right-click **Models** menu.

It works best if you want a simple "how close am I to the limit?" display that is always visible.

## Requirements

- Windows 10 or Windows 11
- Claude Code (CLI or App) installed and authenticated
- Optional: Codex CLI installed and authenticated, if you want Codex usage
- Optional: Google Antigravity installed with its `agy` CLI authenticated, if you want Antigravity usage
- Optional: Grok Build installed with `grok login` completed, if you want Grok usage
- Optional: Cursor Agent installed with `cursor-agent login` completed, if you want Cursor usage

If you use Claude Code through WSL, that is supported too. The monitor can read your Claude Code credentials from Windows or from your WSL environment.

## Install

### This fork's release

Download the latest Windows executable and its checksum directly from this fork:

- [Download the latest `claude-code-usage-monitor.exe`](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/latest/download/claude-code-usage-monitor.exe)
- [Download `SHA256SUMS.txt`](https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/latest/download/SHA256SUMS.txt)

Verify the download in PowerShell:

```powershell
Invoke-WebRequest "https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/latest/download/claude-code-usage-monitor.exe" -OutFile .\claude-code-usage-monitor.exe
Invoke-WebRequest "https://github.com/Kuu44/Claude-Code-Usage-Monitor/releases/latest/download/SHA256SUMS.txt" -OutFile .\SHA256SUMS.txt
$expected = (((Get-Content .\SHA256SUMS.txt | Select-String "claude-code-usage-monitor.exe" | Select-Object -First 1).ToString()) -split "\s+")[0].ToLowerInvariant()
$actual = (Get-FileHash .\claude-code-usage-monitor.exe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "SHA-256 mismatch: expected $expected, got $actual" }
"SHA-256 verified: $actual"
```

The first fork release is v1.7.1. Each future `v*` tag runs the release workflow, validates the package version and Windows executable, creates `SHA256SUMS.txt`, and publishes the two release assets.

### WinGet

The `CodeZeno.ClaudeCodeUsageMonitor` WinGet package is the upstream Code Zeno channel. It installs and updates the upstream project, not this Kuu44 fork. Use the direct fork release links above when you want this fork.

For reference, the upstream WinGet command is:

```powershell
winget install CodeZeno.ClaudeCodeUsageMonitor
```

## Use

After downloading the fork executable, run:

```powershell
.\claude-code-usage-monitor.exe
```

Once running, it will appear in your taskbar and as one or more tray icons in the notification area.

- Drag the left divider to move the taskbar widget
- On multi-monitor setups, drag the widget onto another Windows taskbar to move it to that screen, or pick the screen directly via **Settings → Monitor**
- Right-click the taskbar widget or tray icon for refresh, displayed models, update frequency, Start with Windows, reset position, taskbar side, language, authentication, updates, and exit
- Use **Settings → Taskbar Side** to anchor the widget to the left or right side of the taskbar (right by default); dragging still fine-tunes the position from the chosen side
- Use **Settings → Authentication** to launch the provider's own login command. Choose **Re-login** when a provider shows `!` or its credential has expired. The monitor never handles or stores provider passwords.
- Left-click the tray icon to toggle the taskbar widget on or off
- Enable `Start with Windows` from the right-click menu if you want it to launch automatically when you sign in

### Models

Use the right-click **Models** menu to choose what the widget displays:

- **Claude Code** is enabled by default
- **Codex** can be enabled alongside Claude Code or shown by itself
- **Antigravity** can be enabled alongside the other providers or shown by itself as its own model column
- **Grok** can be enabled alongside the other providers and shows weekly credits in the bottom row only
- **Cursor** can be enabled alongside the other providers and shows monthly plan usage in the bottom row only

When multiple models are shown, each model has its own usage bar and matching usage text color: Claude uses warm orange, Codex uses a monochrome palette, Antigravity uses Google blue, Grok uses teal, and Cursor uses purple. Antigravity prefers Google's Gemini quota summary when available and falls back to model quota data when needed. If Cursor is enabled, the compact bottom label changes from `7d` to `7d/mo`.

### System Tray Icon

Each tray badge uses its provider's native usage window: Claude Code, Codex, and Antigravity show current 5-hour usage; Grok shows weekly credits; and Cursor shows monthly plan usage.

If multiple providers are enabled, the app shows one tray icon per provider. If only one model is enabled, it shows one tray icon.

The Claude Code tray icon uses the same warm usage colors as the Claude bar. The Codex tray icon uses a black and white badge style. The Antigravity tray icon uses a blue badge style. Grok uses teal, and Cursor uses purple.

The Grok tray tooltip reports weekly usage only. The Cursor tray tooltip reports monthly usage only.

Hovering over a tray icon shows the usage values for that model.

## Diagnostics

If you need to troubleshoot startup or visibility issues, run:

```powershell
.\claude-code-usage-monitor.exe --diagnose
```

This writes a log file to:

```text
%TEMP%\claude-code-usage-monitor.log
```

Settings are saved to:

```text
%APPDATA%\ClaudeCodeUsageMonitor\settings.json
```

## Releases And Updates

The fork publishes Windows releases from `v*` tags. The release workflow builds with the pinned Rust toolchain, checks the embedded executable version and static runtime dependencies, and publishes only the expected executable and `SHA256SUMS.txt` asset. It does not submit fork builds to WinGet.

Portable copies automatically check this fork's GitHub Releases feed at startup when no prior check exists or when the last check is more than 24 hours old. You can also invoke the update action manually. A portable copy downloads the validated release executable and replaces itself through the local updater helper. WinGet installs follow their existing upstream Code Zeno channel and are updated by WinGet instead.

## Account Support

This app works with the same account types that Claude Code itself supports.

As of **March 19, 2026**, Anthropic's Claude Code setup documentation says:

- **Supported:** Pro, Max, Teams, Enterprise, and Console accounts
- **Not supported:** the free Claude.ai plan

If Anthropic changes Claude Code availability in the future, this app should follow whatever Claude Code supports, as long as the usage data remains exposed through the same authenticated endpoints.

## Privacy And Security

This project is **open source**, so you can inspect exactly what it does.

What the app reads:

- Your local Claude Code OAuth credentials from `~/.claude/.credentials.json`
- If needed, the same credentials file inside an installed WSL distro
- If Codex is enabled, your local Codex credentials from `$CODEX_HOME/auth.json` or `~/.codex/auth.json`
- If Antigravity is enabled, your local Antigravity OAuth token from Windows Credential Manager target `gemini:antigravity`
- If Grok is enabled, the local Grok Build credential map from `~/.grok/auth.json` (the selected key and user ID are held in memory only)
- If Cursor is enabled, the local Cursor Agent access token from `%APPDATA%\Cursor\auth.json`

What the app sends over the network:

- Requests to Anthropic's Claude endpoints to read your usage and rate-limit information
- Requests to ChatGPT's Codex usage endpoint to read your Codex usage and rate-limit information, if Codex is enabled
- Requests to Google's Cloud Code / Antigravity endpoints to read your Antigravity quota information, if Antigravity is enabled
- Requests to Grok Build's billing endpoint to read weekly credit usage, if Grok is enabled
- Requests to Cursor's DashboardService endpoint to read monthly plan usage, if Cursor is enabled
- Requests to GitHub only if you use the app's update check / self-update feature
- If proxy environment variables such as `HTTPS_PROXY`, `HTTP_PROXY`, or `ALL_PROXY` are set, those outbound requests may use that proxy

What the app stores locally:

- Widget position and taskbar side
- Selected taskbar / screen
- Widget visibility
- Polling frequency
- Language preference
- Last update check time
- Displayed model preferences

What it does **not** do:

- It does not send your credentials to any other server
- It does not use a separate backend service
- It does not collect analytics or telemetry
- It does not upload your project files
- It does not directly edit your Codex credentials file

Notes:

- If your Claude Code token is expired, the app may ask the local Claude CLI to refresh it in the background
- If your Codex token is expired, the app may ask the local Codex CLI to refresh it in the background. The monitor does not write `auth.json` itself; any credential update is handled by the Codex CLI.
- If your Antigravity token is expired, run `agy` in a terminal and sign in again. The monitor does not write Windows Credential Manager entries itself.
- Grok and Cursor usage polling is read-only. It does not launch login prompts, refresh tokens, exchange credentials, or consume credits. Use **Settings → Authentication** to launch `grok login` or `cursor-agent login` when needed.
- Portable installs can update themselves by downloading the latest release from this fork
- Proxies should be trusted because proxied usage requests include your OAuth bearer token inside the TLS connection

## How It Works

The monitor:

1. Finds your enabled model login credentials
2. Reads your current usage from Anthropic, ChatGPT, Google's Antigravity endpoints, Grok Build billing, and/or Cursor DashboardService
3. Shows the result directly in the Windows taskbar
4. Keeps the widget aligned with the selected taskbar and tray area
5. Refreshes periodically in the background

If the newer usage endpoint is unavailable, it can fall back to reading the rate-limit headers returned by Claude's Messages API.

## Open Source

This project is licensed under MIT.

If you want to inspect the behavior or audit the code, everything is in this repository.
