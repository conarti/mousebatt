# Changelog

All notable changes to this project are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Polling rate** submenu in the right-click menu for Pulsar mice: pick 125 Hz–8 kHz
  (only rates the current link supports are listed — 1 kHz max on cable, up to 8 kHz on
  the 8K dongle); the current rate is checked and also shown in the tooltip

## [0.1.0] - 2026-08-23

### Added
- Tray icon showing battery percentage for Pulsar (X3 family, 8K Dongle Gen.2) and VAXEE (XE-S, 4K dongle) mice
- Colour states: normal / charging / low (≤20%) / stale
- Tooltip with model, percentage, charging state and voltage (Pulsar)
- Re-read on device plug/unplug and on resume from sleep; left-click to refresh
- "Start with Windows" toggle in the right-click menu
- Single ~290 KB exe with no dependencies beyond `windows-sys`

[Unreleased]: https://github.com/ryanlewis/mousebatt/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ryanlewis/mousebatt/releases/tag/v0.1.0
