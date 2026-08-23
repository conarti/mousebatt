# mousebatt

A tiny Windows system-tray battery monitor for Pulsar and VAXEE wireless gaming mice.
The current battery percentage is drawn directly onto the tray icon.

- ~300 KB single exe, no runtime, no installer
- Zero CPU while idle — event-driven Win32, no polling threads
- Pure Rust; the only dependency is [`windows-sys`](https://crates.io/crates/windows-sys) (no hidapi, no GUI framework)

## Supported devices

| Vendor | Tested with | Connection |
|---|---|---|
| Pulsar (VID `0x3710`) | X3 LHD CrazyLight mini / Medium | wired and 8K Dongle Gen.2 |
| VAXEE (VID `0x3057`) | XE-S | 4K wireless dongle |

Other mice from these vendors that use the same receivers/firmware will likely work,
since devices are matched by vendor ID + HID usage page rather than specific product IDs.

## What it does

- Polls the mouse every 4 minutes over the vendor HID interface
- Re-reads immediately when a USB device is plugged/unplugged (debounced) and on resume from sleep
- Icon text color: white = normal, green = charging, red = ≤20%, gray = stale/no data
- Tooltip shows model name, percentage, charging state, and battery voltage (Pulsar only)
- Left-click = refresh now; right-click = menu with **Refresh**, **Start with Windows**, **Exit**

If the mouse is asleep and doesn't answer, the last known value is shown in gray and
marked stale in the tooltip — it recovers on the next poll.

## Building

Requires the Rust toolchain with the MSVC target.

```
cargo build --release
```

The binary lands in `target/release/mousebatt.exe`. Run it — it lives entirely in the tray.
Windows 11 hides new tray icons in the overflow flyout by default; drag it onto the
taskbar (or enable it under taskbar settings) to keep it visible.

## Notes on the hardware

These mice estimate charge from battery voltage (no coulomb counting), so the
percentage jumps up when you plug in and sags back when you unplug — that's the
firmware's estimate, not a bug here. Treat readings as roughly ±10%.
VAXEE reports in 5% steps and does not expose voltage.

Polling happens over the wireless link, so the interval defaults to 4 minutes (`POLL_INTERVAL_MS` in `src/main.rs`).

## Protocol credits

The vendor protocols were reverse-engineered by the community:

- Pulsar: same protocol as the Linux [`hid-kysona`](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-kysona.c) driver
  (17-byte report `08 04 … 49`; battery %, charging flag, and voltage in the reply),
  also documented via [jonkristian/pulsar-x3-python](https://github.com/jonkristian/pulsar-x3-python)
- VAXEE: feature-report protocol documented in [stuffz/mouse-battery-monitor](https://github.com/stuffz/mouse-battery-monitor)
