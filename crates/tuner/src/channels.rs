//! ISDB-Tb UHF raster (Argentina/Brazil, 6 MHz) and the channel scan.

use crate::it9175::SignalStatus;
use crate::ts::{Demux, Packetizer};
use crate::usb::{Result, Stick};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const UHF_FIRST: u8 = 14;
pub const UHF_LAST: u8 = 51;

/// Centre frequency of a UHF channel in kHz: 473143 + 6000 * (ch - 14) (the +1/7 MHz ISDB-Tb offset).
pub fn uhf_center_khz(ch: u8) -> u32 {
    473_143 + 6000 * (ch as u32 - 14)
}

/// A service found by the scan: what the channel list shows and saves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    /// Physical UHF channel.
    pub physical: u8,
    pub freq_khz: u32,
    pub service_id: u16,
    pub name: String,
    pub provider: String,
    pub network_name: String,
    pub ts_id: Option<u16>,
    pub service_type: Option<u8>,
    pub one_seg: bool,
    /// "7.1" style number from the NIT remote key, "" when unknown.
    pub virtual_number: String,
    pub video: Option<String>,
    pub audio: Option<String>,
    /// Signal strength (%) when scanned.
    pub strength: Option<u8>,
}

/// Progress of a scan, reported once per physical channel (before and after trying it).
#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub physical: u8,
    pub freq_khz: u32,
    /// 0-based position in the scan and the number of channels to try.
    pub index: usize,
    pub total: usize,
    /// "tuning", "locked", "reading", "empty", "done"
    pub stage: &'static str,
    pub services_found: usize,
    pub signal: Option<SignalStatus>,
}

/// How long to read the TS of a locked multiplex: until PAT, PMTs, SDT and NIT are in, or until the basic tables
/// are in and `BASIC_WAIT` passed, at most `MAX_WAIT`.
const BASIC_WAIT: Duration = Duration::from_secs(4);
const MAX_WAIT: Duration = Duration::from_secs(9);

/// Reads the TS of the currently tuned multiplex and returns its demux. Stops early on `cancel`.
pub fn read_tables(stick: &Stick, cancel: &AtomicBool) -> Result<Demux> {
    let demux = Mutex::new(Demux::new());
    let start = Instant::now();
    let mut pk = Packetizer::default();
    stick.stream(
        |chunk| {
            let mut d = demux.lock().unwrap();
            pk.feed(chunk, |p| d.packet(p));
            true
        },
        || {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let t = start.elapsed();
            let d = demux.lock().unwrap();
            !(d.is_complete() || (d.has_basic_info() && t >= BASIC_WAIT) || t >= MAX_WAIT)
        },
    )?;
    Ok(demux.into_inner().unwrap())
}

/// Turns a demuxed multiplex into channel-list entries (TV services only).
pub fn channels_of(d: &Demux, physical: u8, freq_khz: u32, strength: Option<u8>) -> Vec<Channel> {
    d.services()
        .into_iter()
        .filter(|s| s.is_tv())
        .map(|s| Channel {
            physical,
            freq_khz,
            service_id: s.service_id,
            name: if s.name.is_empty() {
                format!("Service 0x{:04X}", s.service_id)
            } else {
                s.name.clone()
            },
            provider: s.provider.clone(),
            network_name: d.network_name.clone(),
            ts_id: d.ts_id,
            service_type: s.service_type,
            one_seg: s.one_seg,
            virtual_number: s.virtual_number.clone(),
            video: s
                .streams
                .iter()
                .find(|x| x.is_video())
                .map(|x| x.codec().to_string()),
            audio: s
                .streams
                .iter()
                .find(|x| x.is_audio())
                .map(|x| x.codec().to_string()),
            strength,
        })
        .collect()
}

/// Scans UHF channels `from..=to` (stick already initialised). Calls `progress` as it goes; `cancel` stops it.
pub fn scan(
    stick: &Stick,
    from: u8,
    to: u8,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
) -> Result<Vec<Channel>> {
    let mut found = vec![];
    let total = (to - from + 1) as usize;
    for (index, ch) in (from..=to).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let freq = uhf_center_khz(ch);
        let mut p = ScanProgress {
            physical: ch,
            freq_khz: freq,
            index,
            total,
            stage: "tuning",
            services_found: found.len(),
            signal: None,
        };
        progress(p.clone());
        if !stick.tune(freq, true)? {
            p.stage = "empty";
            progress(p);
            continue;
        }
        let sig = stick.status().ok();
        p.stage = "reading";
        p.signal = sig.clone();
        progress(p.clone());
        let d = read_tables(stick, cancel)?;
        found.extend(channels_of(
            &d,
            ch,
            freq,
            sig.as_ref().and_then(|s| s.strength_percent),
        ));
        p.stage = "locked";
        p.services_found = found.len();
        progress(p);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    #[test]
    fn raster() {
        assert_eq!(super::uhf_center_khz(14), 473_143);
        assert_eq!(super::uhf_center_khz(23), 527_143);
        assert_eq!(super::uhf_center_khz(51), 695_143);
    }
}
