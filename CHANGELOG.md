# Changelog

All notable changes to this project are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [SemVer](https://semver.org/).

## [0.2.0] - 2026-09-12

### Added
- **Polling rate** submenu in the right-click menu; the current rate is checked and also
  shown in the tooltip. Only rates the current link supports are listed:
  - Pulsar: 125 Hz–8 kHz (1 kHz max on cable, up to 8 kHz on the 8K dongle)
  - VAXEE: 500 Hz–4 kHz on the VXD02 4K dongle (8 kHz on the 8K receiver), 1 kHz max on
    cable or in a "Standard" tracking mode, matching the VAXEE Control Center

## [0.1.0] - 2026-08-23

### Added
- Tray icon showing battery percentage for Pulsar (X3 family, 8K Dongle Gen.2) and VAXEE (XE-S, 4K dongle) mice
- Colour states: normal / charging / low (≤20%) / stale
- Tooltip with model, percentage, charging state and voltage (Pulsar)
- Re-read on device plug/unplug and on resume from sleep; left-click to refresh
- "Start with Windows" toggle in the right-click menu
- Single ~290 KB exe with no dependencies beyond `windows-sys`

[0.2.0]: https://github.com/ryanlewis/mousebatt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ryanlewis/mousebatt/releases/tag/v0.1.0
