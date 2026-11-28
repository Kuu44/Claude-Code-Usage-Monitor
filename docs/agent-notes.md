# Agent notes

Rules for agents and contributors changing this code.

- Adding a field to `Strings` (`src/localization/mod.rs`) means updating every language file in `src/localization/` (11 languages, one file each, English included). A missing field breaks the build.
- Taskbar positioning math lives in `position_at_taskbar` and in the live-drag handler in `src/window.rs`. Change one and check the other so they stay in sync.
