# Changelog

This changelog records the fork's release sequence after the upstream v1.4.9 baseline. Each user-prompted change receives one version increment, with feature additions using a minor bump and compatible fixes using a patch bump.

## [1.7.1] - 2026-08-27

- Added a consistent provider color system across usage bars, usage text, tray badges, and the Models menu.
- Made model-selector identity easier to scan: Claude is warm orange, Codex is monochrome, Antigravity is blue, Grok is teal, and Cursor is purple.

## [1.7.0]

- Added Grok Build weekly credit usage monitoring.
- Added Cursor monthly plan usage monitoring.

## [1.6.0]

- Added provider-specific login actions for Claude Code, Codex, Antigravity, Grok, and Cursor.
- Added force-relogin actions for stale or expired provider credentials.

## [1.5.2]

- Fixed taskbar widget stability when Windows changes between extended and single-monitor display modes.
- Preserved the widget on the taskbar that remains available after a monitor switch.

## [1.5.1]

- Fixed usage-row placement when a provider does not expose a particular usage window.
- Hid unavailable 5-hour or weekly bars instead of showing a misleading empty `0%` row.

## [1.5.0]

- Added left/right taskbar-side placement controls.
- Added direct monitor/taskbar selection from Settings for multi-monitor setups.
