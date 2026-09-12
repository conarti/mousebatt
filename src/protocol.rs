//! Vendor battery protocols, ported from the working Node.js probes.
//!
//! Pulsar (X3-family, wired or 8K dongle; same as Linux hid-kysona / hid-pulsar):
//!   17-byte OUTPUT reports `08 <cmd> … <ck>` where `ck = 0x55 - sum(bytes 0..16)`.
//!   The reply is an INPUT report echoing the command byte.
//!     cmd 0x01 (info): request byte 5 = 8, bytes 6-9 = nonce;
//!                      reply byte 12 = link type (wired / 1K / 2K / 4K / 8K).
//!     cmd 0x04 (power): reply [6]=battery% [7]=charging [8..9]=voltage mV BE.
//!     cmd 0x07 / 0x08 (mem set / get): bytes 3-4 = address, 5 = length,
//!                      6.. = data. Single-byte settings are stored as the
//!                      pair `(v, 0x55 - v)`; the polling rate lives at 0x0000
//!                      as the report period (0x01 = 1 kHz … 0x40 = 8 kHz).
//!   Register map from packerlschupfer/pulsar-mouse-linux; verified on an
//!   X3 LHD CrazyLight (wired and 8K Dongle Gen.2).
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
    /// Polling-rate state, for mice whose protocol exposes it (Pulsar).
    pub polling: Option<PollingInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollingInfo {
    /// Rate stored in the active profile.
    pub hz: u16,
    /// Highest rate the current link (cable or dongle) supports.
    pub max_hz: u16,
}

pub enum ReadResult {
    Ok(BatteryStatus),
    /// A supported device is present but didn't answer (mouse asleep / out of range).
    NoResponse(String),
    NoDevice,
}

/// Every polling rate the Pulsar protocol can encode, ascending.
pub const PULSAR_RATES: [u16; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

pub fn read_battery() -> ReadResult {
    let devices = enumerate();
    if let Some(info) = find_pulsar(&devices) {
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

/// Write `hz` into the active profile of the connected Pulsar mouse.
/// Returns `true` once the mouse has echoed the write back.
pub fn set_pulsar_polling(hz: u16) -> bool {
    let Some(code) = pulsar_rate_code(hz) else {
        return false;
    };
    let devices = enumerate();
    let Some(info) = find_pulsar(&devices) else {
        return false;
    };
    let Some(dev) = HidDevice::open(info) else {
        return false;
    };
    let req = pulsar_mem_set(
        PULSAR_ADDR_POLLING,
        &[code, PULSAR_MAGIC.wrapping_sub(code)],
    );
    (0..2).any(|_| pulsar_xact(&dev, &req).is_some_and(|resp| pulsar_set_acked(&req, &resp)))
}

fn find_pulsar(devices: &[HidDeviceInfo]) -> Option<&HidDeviceInfo> {
    devices
        .iter()
        .find(|d| d.vid == PULSAR_VID && d.usage_page == 0xff02)
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
    // The battery query doubles as the wake-up probe: a sleeping mouse ignores
    // the first attempt, so try twice before giving up.
    let req = pulsar_frame(PULSAR_CMD_POWER, &[]);
    let status = (0..2)
        .find_map(|_| pulsar_xact(&dev, &req))
        .and_then(|resp| parse_pulsar(&resp, &info.product));
    let Some(mut status) = status else {
        return ReadResult::NoResponse(info.product.clone());
    };
    // Best effort: the tray still works without it.
    status.polling = pulsar_polling(&dev);
    ReadResult::Ok(status)
}

/// Send one frame and wait for the reply that echoes its command byte.
/// Other input reports may share this collection (or a late reply to a
/// previous attempt may land first), so keep reading until the deadline.
fn pulsar_xact(dev: &HidDevice, req: &[u8; 17]) -> Option<Vec<u8>> {
    if !dev.write(req) {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(2500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let resp = dev.read_timeout(remaining.as_millis() as u32)?;
        if resp.len() >= 2 && resp[0] == PULSAR_REPORT_ID && resp[1] == req[1] {
            return Some(resp);
        }
    }
}

fn pulsar_polling(dev: &HidDevice) -> Option<PollingInfo> {
    let info = pulsar_xact(dev, &pulsar_info_request())?;
    let max_hz = parse_pulsar_link_max(&info)?;
    let mem = pulsar_xact(dev, &pulsar_mem_get(PULSAR_ADDR_POLLING, 2))?;
    let hz = parse_pulsar_polling(&mem)?;
    Some(PollingInfo { hz, max_hz })
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

const PULSAR_REPORT_ID: u8 = 0x08;
const PULSAR_CMD_INFO: u8 = 0x01;
const PULSAR_CMD_POWER: u8 = 0x04;
const PULSAR_CMD_MEM_SET: u8 = 0x07;
const PULSAR_CMD_MEM_GET: u8 = 0x08;
/// Frame checksum complement; also used for the `(v, 0x55 - v)` setting pairs.
const PULSAR_MAGIC: u8 = 0x55;
const PULSAR_ADDR_POLLING: u16 = 0x0000;

const VAXEE_CMD_BATTERY: u8 = 0x0b;
const VAXEE_CMD_CHARGING: u8 = 0x10;

/// Build a 17-byte Pulsar frame: report ID, command, `body` from byte 2, and
/// the checksum in byte 16.
fn pulsar_frame(cmd: u8, body: &[u8]) -> [u8; 17] {
    let mut f = [0u8; 17];
    f[0] = PULSAR_REPORT_ID;
    f[1] = cmd;
    f[2..2 + body.len()].copy_from_slice(body);
    let sum = f[..16].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    f[16] = PULSAR_MAGIC.wrapping_sub(sum);
    f
}

/// Link/model query. The nonce is echoed back transformed; we don't check it,
/// the reply's byte 12 is what we're after.
fn pulsar_info_request() -> [u8; 17] {
    pulsar_frame(PULSAR_CMD_INFO, &[0, 0, 0, 8, 0x11, 0x22, 0x33, 0x44])
}

fn pulsar_mem_get(addr: u16, len: u8) -> [u8; 17] {
    let [hi, lo] = addr.to_be_bytes();
    pulsar_frame(PULSAR_CMD_MEM_GET, &[0, hi, lo, len])
}

fn pulsar_mem_set(addr: u16, data: &[u8]) -> [u8; 17] {
    let [hi, lo] = addr.to_be_bytes();
    let mut body = vec![0, hi, lo, data.len() as u8];
    body.extend_from_slice(data);
    pulsar_frame(PULSAR_CMD_MEM_SET, &body)
}

/// A mem-set is acknowledged by echoing command, address, length and data.
fn pulsar_set_acked(req: &[u8; 17], resp: &[u8]) -> bool {
    let n = 6 + req[5] as usize;
    resp.len() >= n && resp[1..n] == req[1..n]
}

/// Report-period code for a polling rate: 1 kHz = 1, slower rates are the
/// period in ms, faster ones scale up from 0x10 = 2 kHz.
fn pulsar_rate_code(hz: u16) -> Option<u8> {
    Some(match hz {
        125 => 0x08,
        250 => 0x04,
        500 => 0x02,
        1000 => 0x01,
        2000 => 0x10,
        4000 => 0x20,
        8000 => 0x40,
        _ => return None,
    })
}

fn pulsar_code_rate(code: u8) -> Option<u16> {
    PULSAR_RATES
        .iter()
        .copied()
        .find(|&hz| pulsar_rate_code(hz) == Some(code))
}

/// Highest polling rate the link in an info reply supports.
fn parse_pulsar_link_max(resp: &[u8]) -> Option<u16> {
    if resp.len() < 13 || resp[0] != PULSAR_REPORT_ID || resp[1] != PULSAR_CMD_INFO {
        return None;
    }
    // Same table as Pulsar's web driver (Bibimbap) and hid-pulsar's CON_* enum.
    Some(match resp[12] {
        0 => 1000, // 1K dongle
        1 => 4000, // 4K dongle
        2 => 1000, // wired
        3 => 8000, // wired, 8K-capable
        4 => 2000, // 2K dongle
        5 => 8000, // 8K dongle
        _ => return None,
    })
}

/// Polling rate from a mem-get reply for address 0x0000: the stored code and
/// its `0x55 - code` complement must agree.
fn parse_pulsar_polling(resp: &[u8]) -> Option<u16> {
    if resp.len() < 8
        || resp[0] != PULSAR_REPORT_ID
        || resp[1] != PULSAR_CMD_MEM_GET
        || resp[3..5] != PULSAR_ADDR_POLLING.to_be_bytes()
        || resp[7] != PULSAR_MAGIC.wrapping_sub(resp[6])
    {
        return None;
    }
    pulsar_code_rate(resp[6])
}

/// Decode a Pulsar input report. `None` if it isn't a battery reply.
fn parse_pulsar(resp: &[u8], product: &str) -> Option<BatteryStatus> {
    if resp.len() < 10 || resp[0] != PULSAR_REPORT_ID || resp[1] != PULSAR_CMD_POWER {
        return None;
    }
    Some(BatteryStatus {
        percent: resp[6].min(100),
        charging: resp[7] == 1,
        voltage_mv: Some(u16::from_be_bytes([resp[8], resp[9]])),
        product: product.to_string(),
        polling: None,
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
        polling: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulsar_power_frame_matches_kysona() {
        let r = pulsar_frame(PULSAR_CMD_POWER, &[]);
        assert_eq!(r[0], 0x08);
        assert_eq!(r[1], 0x04);
        assert!(r[2..16].iter().all(|&b| b == 0));
        assert_eq!(r[16], 0x49);
        assert_eq!(r.iter().map(|&b| b as u32).sum::<u32>(), 0x55);
    }

    #[test]
    fn pulsar_checksum_wraps() {
        // Captured: `08 08 00 00 00 0a 00*10 3b` and `08 07 00 00 00 02 02 53 … ef`.
        assert_eq!(pulsar_mem_get(0, 10)[16], 0x3b);
        let set = pulsar_mem_set(0, &[0x02, 0x53]);
        assert_eq!(&set[..8], &[0x08, 0x07, 0, 0, 0, 2, 0x02, 0x53]);
        assert_eq!(set[16], 0xef);
        // 16-bit addresses split big-endian into bytes 3-4.
        assert_eq!(&pulsar_mem_get(0x1b32, 4)[2..6], &[0, 0x1b, 0x32, 4]);
    }

    #[test]
    fn pulsar_info_request_frame() {
        // Captured: `08 01 00 00 00 08 11 22 33 44 00*6 9a`.
        let r = pulsar_info_request();
        assert_eq!(&r[..10], &[0x08, 0x01, 0, 0, 0, 8, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(r[16], 0x9a);
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
        assert_eq!(s.polling, None);
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
    fn pulsar_rate_codes_round_trip() {
        for hz in PULSAR_RATES {
            assert_eq!(pulsar_code_rate(pulsar_rate_code(hz).unwrap()), Some(hz));
        }
        assert_eq!(pulsar_rate_code(1000), Some(0x01));
        assert_eq!(pulsar_rate_code(125), Some(0x08));
        assert_eq!(pulsar_rate_code(8000), Some(0x40));
        assert_eq!(pulsar_rate_code(3000), None);
        assert_eq!(pulsar_code_rate(0x03), None);
    }

    fn info_reply(link: u8) -> Vec<u8> {
        // Captured wired reply: `08 01 00 00 00 08 8a df dd 21 57 68 02 00 00 00 1c`.
        let mut r = vec![
            0x08, 0x01, 0, 0, 0, 8, 0x8a, 0xdf, 0xdd, 0x21, 0x57, 0x68, 0x02, 0, 0, 0, 0x1c,
        ];
        r[12] = link;
        r
    }

    #[test]
    fn pulsar_link_max_rate() {
        assert_eq!(parse_pulsar_link_max(&info_reply(2)), Some(1000));
        assert_eq!(parse_pulsar_link_max(&info_reply(0)), Some(1000));
        assert_eq!(parse_pulsar_link_max(&info_reply(1)), Some(4000));
        assert_eq!(parse_pulsar_link_max(&info_reply(3)), Some(8000));
        assert_eq!(parse_pulsar_link_max(&info_reply(4)), Some(2000));
        assert_eq!(parse_pulsar_link_max(&info_reply(5)), Some(8000));
        assert_eq!(parse_pulsar_link_max(&info_reply(9)), None);
        assert_eq!(parse_pulsar_link_max(&info_reply(2)[..12]), None);
        let mut wrong_cmd = info_reply(2);
        wrong_cmd[1] = 0x04;
        assert_eq!(parse_pulsar_link_max(&wrong_cmd), None);
    }

    fn mem_reply(code: u8, complement: u8) -> Vec<u8> {
        // Captured: `08 08 00 00 00 0a 01 54 02 53 01 54 00 55 00 55 92`.
        let mut r = vec![0x08, 0x08, 0, 0, 0, 2, code, complement];
        r.resize(17, 0);
        r
    }

    #[test]
    fn pulsar_polling_from_mem_reply() {
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x54)), Some(1000));
        assert_eq!(parse_pulsar_polling(&mem_reply(0x02, 0x53)), Some(500));
        assert_eq!(parse_pulsar_polling(&mem_reply(0x40, 0x15)), Some(8000));
        // complement mismatch, unknown code, wrong address, short
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x00)), None);
        assert_eq!(parse_pulsar_polling(&mem_reply(0x03, 0x52)), None);
        let mut other_addr = mem_reply(0x01, 0x54);
        other_addr[4] = 0x02;
        assert_eq!(parse_pulsar_polling(&other_addr), None);
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x54)[..7]), None);
    }

    #[test]
    fn pulsar_set_ack_requires_echo() {
        let req = pulsar_mem_set(0, &[0x02, 0x53]);
        assert!(pulsar_set_acked(&req, &req));
        let mut other_value = req;
        other_value[6] = 0x01;
        other_value[7] = 0x54;
        assert!(!pulsar_set_acked(&req, &other_value));
        assert!(!pulsar_set_acked(&req, &req[..7]));
        // Trailing bytes/checksum are not part of the ack.
        let mut junk_tail = req;
        junk_tail[16] = 0;
        assert!(pulsar_set_acked(&req, &junk_tail));
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
        assert_eq!(s.polling, None);
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
