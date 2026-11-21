//! Keeps only one service of a multi-service ISDB-T transport stream: passes that service's PMT, PCR, video and
//! audio, drops the original PAT and injects a single-program PAT instead, so a stock TS player (mpegts.js, ffplay)
//! locks onto the chosen service. Port of OpenDTV's ServiceFilter, minus captions.

use super::{crc32_mpeg, pid, Demux, PID_PAT, PKT};
use std::collections::HashSet;

/// Passed packets between PAT insertions.
const PAT_INTERVAL: u32 = 100;

pub struct ServiceFilter {
    sid: u16,
    pass: HashSet<u16>,
    ready: bool,
    pat: [u8; PKT],
    pat_cc: u8,
    since_pat: u32,
}

impl ServiceFilter {
    pub fn new(service_id: u16) -> Self {
        ServiceFilter {
            sid: service_id,
            pass: HashSet::new(),
            ready: false,
            pat: [0xFF; PKT],
            pat_cc: 0,
            since_pat: PAT_INTERVAL,
        }
    }

    pub fn service_id(&self) -> u16 {
        self.sid
    }

    /// True once the service's PMT has been seen and packets are being passed.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Refreshes the PID set from the demux (call after feeding it; cheap).
    pub fn update(&mut self, demux: &Demux) {
        let Some(s) = demux.service(self.sid) else {
            return;
        };
        let (Some(pmt), true) = (s.pmt_pid, s.pmt_seen) else {
            return;
        };
        let mut pass = HashSet::new();
        pass.insert(pmt);
        if let Some(pcr) = s.pcr_pid {
            pass.insert(pcr);
        }
        for st in &s.streams {
            if st.is_video() || st.is_audio() {
                pass.insert(st.pid);
            }
        }
        if !self.ready || pass != self.pass {
            self.build_pat(demux.ts_id.unwrap_or(1), pmt);
            self.pass = pass;
            self.ready = true;
        }
    }

    fn build_pat(&mut self, ts_id: u16, pmt_pid: u16) {
        let mut s = [0u8; 16];
        s[0] = 0x00; // table_id
        s[1] = 0xB0; // syntax=1, length hi = 0
        s[2] = 13; // section_length
        s[3..5].copy_from_slice(&ts_id.to_be_bytes());
        s[5] = 0xC1; // version 0, current
        s[8..10].copy_from_slice(&self.sid.to_be_bytes());
        s[10] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        s[11] = pmt_pid as u8;
        let crc = crc32_mpeg(&s[..12]);
        s[12..16].copy_from_slice(&crc.to_be_bytes());
        let pk = &mut self.pat;
        pk.fill(0xFF);
        pk[0] = 0x47;
        pk[1] = 0x40; // payload_unit_start, PID 0
        pk[2] = 0x00;
        pk[4] = 0; // pointer_field
        pk[5..21].copy_from_slice(&s);
        self.since_pat = PAT_INTERVAL;
    }

    /// Filters one aligned packet, appending the output (0, 1 or 2 packets) to `out`.
    pub fn packet(&mut self, b: &[u8], out: &mut Vec<u8>) {
        if !self.ready {
            return;
        }
        let p = pid(b);
        if p == PID_PAT || !self.pass.contains(&p) {
            return;
        }
        if self.since_pat >= PAT_INTERVAL {
            self.pat[3] = 0x10 | (self.pat_cc & 0x0F);
            self.pat_cc = self.pat_cc.wrapping_add(1);
            out.extend_from_slice(&self.pat);
            self.since_pat = 0;
        }
        self.since_pat += 1;
        out.extend_from_slice(b);
    }
}
