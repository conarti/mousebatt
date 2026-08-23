//! Vendor battery protocols, ported from the working Node.js probes.
//!
//! Pulsar (X3-family, wired or 8K dongle; same as Linux hid-kysona):
//!   17-byte OUTPUT report `08 04 00*14 49` -> INPUT report with
//!   [6]=battery% [7]=charging [8..9]=voltage mV big-endian.
//!
//! VAXEE (4K dongle; from stuffz/mouse-battery-monitor):
//!   64-byte FEATURE reports, report ID 0x0E, header 0xA5.
//!   cmd 0x0B -> resp[5]*5 = battery %, cmd 0x10 -> resp[5]!=0 = charging.
//!   Response valid when resp[2] echoes non-zero.
use crate::hid::{enumerate, HidDevice, HidDeviceInfo, PULSAR_VID, VAXEE_VID};
use std::time::{Duration, Instant};

pub struct BatteryStatus {
    pub percent: u8,
    pub charging: bool,
    pub voltage_mv: Option<u16>,
    pub product: String,
}

pub enum ReadResult {
    Ok(BatteryStatus),
    /// A supported device is present but didn't answer (mouse asleep / out of range).
    NoResponse(String),
    NoDevice,
}

pub fn read_battery() -> ReadResult {
    let devices = enumerate();
    if let Some(info) = devices
        .iter()
        .find(|d| d.vid == PULSAR_VID && d.usage_page == 0xff02)
    {
        return pulsar_read(&with_vendor("Pulsar", info));
    }
    if let Some(info) = devices
        .iter()
        .find(|d| d.vid == VAXEE_VID && d.usage_page == 0xff05)
    {
        return vaxee_read(&with_vendor("VAXEE", info));
    }
    ReadResult::NoDevice
}

/// The HID product string is often just the receiver name ("8K Dongle Gen.2"),
/// so prefix the vendor for the tooltip unless it already names it.
fn with_vendor(vendor: &str, info: &HidDeviceInfo) -> HidDeviceInfo {
    let product = if info
        .product
        .to_ascii_lowercase()
        .contains(&vendor.to_ascii_lowercase())
    {
        info.product.clone()
    } else {
        format!("{vendor} {}", info.product)
    };
    HidDeviceInfo {
        product,
        path: info.path.clone(),
        ..*info
    }
}

fn pulsar_read(info: &HidDeviceInfo) -> ReadResult {
    let Some(dev) = HidDevice::open(info) else {
        return ReadResult::NoResponse(info.product.clone());
    };
    let req = pulsar_request();
    for _ in 0..2 {
        if !dev.write(&req) {
            continue;
        }
        // Other input reports may share this collection (or a late reply to the
        // previous attempt may land first); keep reading until the deadline.
        let deadline = Instant::now() + Duration::from_millis(2500);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(resp) = dev.read_timeout(remaining.as_millis() as u32) else {
                break;
            };
            if let Some(s) = parse_pulsar(&resp, &info.product) {
                return ReadResult::Ok(s);
            }
        }
    }
    ReadResult::NoResponse(info.product.clone())
}

fn vaxee_read(info: &HidDeviceInfo) -> ReadResult {
    let Some(dev) = HidDevice::open(info) else {
        return ReadResult::NoResponse(info.product.clone());
    };
    let query = |cmd: u8| -> Option<Vec<u8>> {
        if !dev.set_feature(&vaxee_request(cmd)) {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        vaxee_valid(dev.get_feature(0x0e)?)
    };
    for _ in 0..2 {
        if let (Some(bat), Some(chg)) = (query(VAXEE_CMD_BATTERY), query(VAXEE_CMD_CHARGING)) {
            return ReadResult::Ok(parse_vaxee(&bat, &chg, &info.product));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    ReadResult::NoResponse(info.product.clone())
}

// ---------------------------------------------------------------------------
// Pure framing/parsing — no HID access, covered by unit tests below.

const VAXEE_CMD_BATTERY: u8 = 0x0b;
const VAXEE_CMD_CHARGING: u8 = 0x10;

/// 17-byte Pulsar battery query (`08 04 00*14 49`; bytes sum to 0x55).
fn pulsar_request() -> [u8; 17] {
    let mut req = [0u8; 17];
    req[0] = 0x08;
    req[1] = 0x04;
    req[16] = 0x49;
    req
}

/// Decode a Pulsar input report. `None` if it isn't a battery reply.
fn parse_pulsar(resp: &[u8], product: &str) -> Option<BatteryStatus> {
    if resp.len() < 10 || resp[0] != 0x08 || resp[1] != 0x04 {
        return None;
    }
    Some(BatteryStatus {
        percent: resp[6].min(100),
        charging: resp[7] == 1,
        voltage_mv: Some(u16::from_be_bytes([resp[8], resp[9]])),
        product: product.to_string(),
    })
}

/// 5-byte VAXEE feature request: report ID, header, cmd, read, length.
fn vaxee_request(cmd: u8) -> [u8; 5] {
    [0x0e, 0xa5, cmd, 0x01, 0x01]
}

/// A VAXEE reply is valid only when the mouse echoed a non-zero cmd byte.
fn vaxee_valid(resp: Vec<u8>) -> Option<Vec<u8>> {
    if resp.len() < 6 || resp[2] == 0 {
        return None;
    }
    Some(resp)
}

/// Combine validated battery and charging replies.
fn parse_vaxee(bat: &[u8], chg: &[u8], product: &str) -> BatteryStatus {
    BatteryStatus {
        percent: ((bat[5] as u16) * 5).min(100) as u8,
        charging: chg[5] != 0,
        voltage_mv: None,
        product: product.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulsar_request_frame() {
        let r = pulsar_request();
        assert_eq!(r[0], 0x08);
        assert_eq!(r[1], 0x04);
        assert!(r[2..16].iter().all(|&b| b == 0));
        assert_eq!(r[16], 0x49);
        assert_eq!(r.iter().map(|&b| b as u32).sum::<u32>(), 0x55);
    }

    fn pulsar_reply(pct: u8, chg: u8, mv: u16) -> Vec<u8> {
        let mut r = vec![0u8; 17];
        r[0] = 0x08;
        r[1] = 0x04;
        r[6] = pct;
        r[7] = chg;
        r[8..10].copy_from_slice(&mv.to_be_bytes());
        r
    }

    #[test]
    fn pulsar_parses_valid_reply() {
        let s = parse_pulsar(&pulsar_reply(73, 0, 3912), "X3").unwrap();
        assert_eq!(s.percent, 73);
        assert!(!s.charging);
        assert_eq!(s.voltage_mv, Some(3912));
        assert_eq!(s.product, "X3");
    }

    #[test]
    fn pulsar_charging_and_voltage_endianness() {
        let s = parse_pulsar(&pulsar_reply(50, 1, 0x1234), "X3").unwrap();
        assert!(s.charging);
        assert_eq!(s.voltage_mv, Some(0x1234));
        // charging flag is exactly 1, not "non-zero"
        assert!(
            !parse_pulsar(&pulsar_reply(50, 2, 0), "X3")
                .unwrap()
                .charging
        );
    }

    #[test]
    fn pulsar_clamps_percent() {
        assert_eq!(
            parse_pulsar(&pulsar_reply(200, 0, 0), "X3")
                .unwrap()
                .percent,
            100
        );
    }

    #[test]
    fn pulsar_rejects_wrong_header_or_short() {
        let mut r = pulsar_reply(50, 0, 0);
        r[1] = 0x05;
        assert!(parse_pulsar(&r, "X3").is_none());
        assert!(parse_pulsar(&pulsar_reply(50, 0, 0)[..9], "X3").is_none());
        assert!(parse_pulsar(&[], "X3").is_none());
    }

    #[test]
    fn vaxee_request_frame() {
        assert_eq!(vaxee_request(0x0b), [0x0e, 0xa5, 0x0b, 0x01, 0x01]);
        assert_eq!(vaxee_request(0x10)[2], 0x10);
    }

    fn vaxee_reply(cmd: u8, val: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x0e;
        r[1] = 0xa5;
        r[2] = cmd;
        r[5] = val;
        r
    }

    #[test]
    fn vaxee_valid_requires_echoed_cmd_and_length() {
        assert!(vaxee_valid(vaxee_reply(0x0b, 10)).is_some());
        assert!(vaxee_valid(vaxee_reply(0x00, 10)).is_none());
        assert!(vaxee_valid(vec![0x0e, 0xa5, 0x0b, 1, 1]).is_none());
    }

    #[test]
    fn vaxee_parses_steps_and_charging() {
        let s = parse_vaxee(&vaxee_reply(0x0b, 17), &vaxee_reply(0x10, 0), "VAXEE");
        assert_eq!(s.percent, 85);
        assert!(!s.charging);
        assert_eq!(s.voltage_mv, None);
        let s = parse_vaxee(&vaxee_reply(0x0b, 20), &vaxee_reply(0x10, 7), "VAXEE");
        assert_eq!(s.percent, 100);
        assert!(s.charging);
    }

    #[test]
    fn vaxee_clamps_percent() {
        assert_eq!(
            parse_vaxee(&vaxee_reply(0x0b, 25), &vaxee_reply(0x10, 0), "V").percent,
            100
        );
    }
}
