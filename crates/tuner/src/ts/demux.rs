//! PSI/SI demux: learns the services of a multiplex from PAT, PMT, SDT and NIT.

use super::section::SectionAssembler;
use super::{
    decode_text, parse_time_section, pid, BroadcastTime, PID_NIT, PID_NULL, PID_PAT, PID_SDT,
    PID_TDT,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One elementary stream listed in a PMT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
}

impl Stream {
    pub fn is_video(&self) -> bool {
        matches!(self.stream_type, 0x01 | 0x02 | 0x10 | 0x1B | 0x24)
    }

    pub fn is_audio(&self) -> bool {
        matches!(self.stream_type, 0x03 | 0x04 | 0x0F | 0x11 | 0x81)
    }

    pub fn codec(&self) -> &'static str {
        match self.stream_type {
            0x01 => "MPEG-1",
            0x02 => "MPEG-2",
            0x1B => "H.264",
            0x24 => "HEVC",
            0x10 => "MPEG-4",
            0x0F => "AAC",
            0x11 => "AAC-LATM",
            0x03 | 0x04 => "MPEG Audio",
            0x81 => "AC-3",
            _ => "other",
        }
    }
}

/// A service (channel) learned from PAT/PMT/SDT/NIT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub service_id: u16,
    pub pmt_pid: Option<u16>,
    pub pcr_pid: Option<u16>,
    /// SDT service_type: 0x01 digital TV, 0xC0 data/one-seg, ...
    pub service_type: Option<u8>,
    pub name: String,
    pub provider: String,
    pub pmt_seen: bool,
    pub streams: Vec<Stream>,
    /// One-seg (partial reception) service: NIT partial_reception_descriptor, PMT PID 0x1FC8..0x1FCF, or the
    /// ISDB-T service_id type bits (bits 4..3 = 0b11).
    pub one_seg: bool,
    /// remote_control_key_id from the NIT TS information descriptor (the "virtual channel" major number).
    pub remote_key: Option<u8>,
    /// Display number such as "7.1" (remote key . index among TV services), or "" when unknown.
    pub virtual_number: String,
}

impl Service {
    fn new(sid: u16) -> Self {
        Service {
            service_id: sid,
            pmt_pid: None,
            pcr_pid: None,
            service_type: None,
            name: String::new(),
            provider: String::new(),
            pmt_seen: false,
            streams: vec![],
            one_seg: false,
            remote_key: None,
            virtual_number: String::new(),
        }
    }

    pub fn has_video(&self) -> bool {
        self.streams.iter().any(Stream::is_video)
    }

    pub fn is_tv(&self) -> bool {
        self.has_video() || self.service_type == Some(0x01)
    }
}

pub struct Demux {
    assemblers: HashMap<u16, SectionAssembler>,
    services: BTreeMap<u16, Service>,
    remote_keys: HashMap<u16, u8>,
    partial: HashSet<u16>,
    pub ts_id: Option<u16>,
    pub network_id: Option<u16>,
    pub network_name: String,
    pub pat_seen: bool,
    pub sdt_seen: bool,
    pub nit_seen: bool,
    pub packets: u64,
    pub tei_packets: u64,
    pub cc_errors: u64,
    /// Latest broadcast date/time (TDT/TOT) and how many have been received.
    pub time: Option<BroadcastTime>,
    pub time_count: u64,
    last_cc: Vec<i8>,
}

impl Default for Demux {
    fn default() -> Self {
        Self::new()
    }
}

impl Demux {
    pub fn new() -> Self {
        let mut assemblers = HashMap::new();
        for p in [PID_PAT, PID_NIT, PID_SDT, PID_TDT] {
            assemblers.insert(p, SectionAssembler::new());
        }
        Demux {
            assemblers,
            services: BTreeMap::new(),
            remote_keys: HashMap::new(),
            partial: HashSet::new(),
            ts_id: None,
            network_id: None,
            network_name: String::new(),
            pat_seen: false,
            sdt_seen: false,
            nit_seen: false,
            packets: 0,
            tei_packets: 0,
            cc_errors: 0,
            time: None,
            time_count: 0,
            last_cc: vec![-1; 0x2000],
        }
    }

    /// Feeds one aligned 188-byte packet.
    pub fn packet(&mut self, b: &[u8]) {
        self.packets += 1;
        let p = pid(b);
        if p == PID_NULL {
            return;
        }
        if b[1] & 0x80 != 0 {
            self.tei_packets += 1;
            return;
        }
        let pusi = b[1] & 0x40 != 0;
        let afc = (b[3] >> 4) & 3;
        let cc = (b[3] & 0x0F) as i8;
        let mut o = 4;
        let mut discontinuity = false;
        if afc & 2 != 0 {
            let afl = b[4] as usize;
            if afl > 0 && 5 < b.len() {
                discontinuity = b[5] & 0x80 != 0;
            }
            o += 1 + afl;
        }
        if afc & 1 == 0 || o >= b.len() {
            return;
        }
        let last = self.last_cc[p as usize];
        if last >= 0 && !discontinuity && cc != last && cc != ((last + 1) & 0x0F) {
            self.cc_errors += 1;
        }
        self.last_cc[p as usize] = cc;

        let Some(mut a) = self.assemblers.remove(&p) else {
            return;
        };
        let mut sections: Vec<Vec<u8>> = vec![];
        a.push(&b[o..], pusi, &mut |s| sections.push(s.to_vec()));
        self.assemblers.insert(p, a);
        for s in sections {
            self.section(p, &s);
        }
    }

    fn section(&mut self, p: u16, s: &[u8]) {
        if p == PID_TDT {
            if let Some(t) = parse_time_section(s) {
                self.time = Some(t);
                self.time_count += 1;
            }
            return;
        }
        if s.len() < 12 {
            return;
        }
        let n = s.len();
        match s[0] {
            0x00 if p == PID_PAT => self.parse_pat(s, n),
            0x02 => self.parse_pmt(p, s, n),
            0x40 if p == PID_NIT => self.parse_nit(s, n),
            0x42 if p == PID_SDT => self.parse_sdt(s, n),
            _ => {}
        }
    }

    fn svc(&mut self, sid: u16) -> &mut Service {
        self.services
            .entry(sid)
            .or_insert_with(|| Service::new(sid))
    }

    fn parse_pat(&mut self, s: &[u8], n: usize) {
        self.ts_id = Some(u16be(s, 3));
        let mut p = 8;
        while p + 4 <= n - 4 {
            let program = u16be(s, p);
            let pmt = u13(s, p + 2);
            p += 4;
            if program == 0 {
                continue; // NIT PID entry
            }
            self.svc(program).pmt_pid = Some(pmt);
            self.assemblers
                .entry(pmt)
                .or_insert_with(SectionAssembler::new);
        }
        self.pat_seen = true;
    }

    fn parse_pmt(&mut self, p: u16, s: &[u8], n: usize) {
        let sid = u16be(s, 3);
        let Some(si) = self.services.get_mut(&sid) else {
            return;
        };
        if si.pmt_pid != Some(p) {
            return;
        }
        si.pcr_pid = Some(u13(s, 8));
        let end = n - 4;
        let mut q = 12 + u12(s, 10);
        let mut streams = vec![];
        while q + 5 <= end {
            let st = Stream {
                stream_type: s[q],
                pid: u13(s, q + 1),
            };
            q = (q + 5 + u12(s, q + 3)).min(end);
            streams.push(st);
        }
        si.streams = streams;
        si.pmt_seen = true;
    }

    fn parse_sdt(&mut self, s: &[u8], n: usize) {
        // Only the actual TS (table 0x42); ts_id from the SDT matches the PAT.
        let end = n - 4;
        let mut p = 11;
        while p + 5 <= end {
            let sid = u16be(s, p);
            let d_end = (p + 5 + u12(s, p + 3)).min(end);
            let mut d = p + 5;
            while d + 2 <= d_end {
                let (tag, dl) = (s[d], s[d + 1] as usize);
                if tag == 0x48 && dl >= 3 && d + 2 + dl <= d_end {
                    let body = &s[d + 2..d + 2 + dl];
                    let pl = body[1] as usize;
                    if 2 + pl < body.len() {
                        let nl = body[2 + pl] as usize;
                        let si = self.svc(sid);
                        si.service_type = Some(body[0]);
                        si.provider = decode_text(&body[2..2 + pl]);
                        let ns = 3 + pl;
                        si.name = decode_text(&body[ns..(ns + nl).min(body.len())]);
                    }
                }
                d += 2 + dl;
            }
            p = d_end;
        }
        self.sdt_seen = true;
    }

    fn parse_nit(&mut self, s: &[u8], n: usize) {
        self.network_id = Some(u16be(s, 3));
        let end = n - 4;
        let mut p = 8;
        let nd_end = (p + 2 + u12(s, p)).min(end);
        p += 2;
        let mut d = p;
        while d + 2 <= nd_end {
            let (tag, dl) = (s[d], s[d + 1] as usize);
            if tag == 0x40 && d + 2 + dl <= nd_end {
                self.network_name = decode_text(&s[d + 2..d + 2 + dl]);
            }
            d += 2 + dl;
        }
        p = nd_end;
        if p + 2 > end {
            return;
        }
        let loop_end = (p + 2 + u12(s, p)).min(end);
        p += 2;
        while p + 6 <= loop_end {
            let tsid = u16be(s, p);
            let td_end = (p + 6 + u12(s, p + 4)).min(loop_end);
            let mut d = p + 6;
            while d + 2 <= td_end {
                let (tag, dl) = (s[d], s[d + 1] as usize);
                if d + 2 + dl > td_end {
                    break;
                }
                let body = &s[d + 2..d + 2 + dl];
                match tag {
                    // TS information descriptor: remote_control_key_id
                    0xCD if dl >= 1 => {
                        self.remote_keys.insert(tsid, body[0]);
                    }
                    // partial reception descriptor: list of one-seg service ids
                    0xFB => {
                        for c in body.as_chunks::<2>().0 {
                            self.partial.insert(u16::from_be_bytes(*c));
                        }
                    }
                    _ => {}
                }
                d += 2 + dl;
            }
            p = td_end;
        }
        self.nit_seen = true;
    }

    /// PAT, every PMT and the SDT have been seen (the NIT is slower and optional).
    pub fn has_basic_info(&self) -> bool {
        self.pat_seen
            && self.sdt_seen
            && self
                .services
                .values()
                .filter(|s| s.pmt_pid.is_some())
                .all(|s| s.pmt_seen)
    }

    pub fn is_complete(&self) -> bool {
        self.has_basic_info() && self.nit_seen
    }

    /// Raw service (no derived fields), e.g. for the filter.
    pub fn service(&self, sid: u16) -> Option<&Service> {
        self.services.get(&sid)
    }

    /// Snapshot of the services with derived fields (one-seg flag, remote key, virtual number) filled in. Only
    /// services listed in the PAT are returned (the SDT can mention services that are not on air).
    pub fn services(&self) -> Vec<Service> {
        let rk = self
            .ts_id
            .and_then(|t| self.remote_keys.get(&t).copied())
            .or_else(|| {
                if self.remote_keys.len() == 1 {
                    self.remote_keys.values().next().copied()
                } else {
                    None
                }
            });
        let tv_count = self
            .services
            .values()
            .filter(|s| s.pmt_pid.is_some() && s.is_tv())
            .count();
        let mut idx = 0;
        let mut out = vec![];
        for si in self.services.values().filter(|s| s.pmt_pid.is_some()) {
            let mut c = si.clone();
            c.remote_key = rk;
            c.one_seg = self.partial.contains(&si.service_id)
                || si.pmt_pid.is_some_and(|p| (0x1FC8..=0x1FCF).contains(&p))
                || (si.service_id >> 3) & 3 == 3;
            if let (Some(rk), true) = (rk, si.is_tv()) {
                idx += 1;
                c.virtual_number = if tv_count > 1 {
                    format!("{rk}.{idx}")
                } else {
                    rk.to_string()
                };
            }
            out.push(c);
        }
        out
    }
}

fn u16be(b: &[u8], o: usize) -> u16 {
    ((b[o] as u16) << 8) | b[o + 1] as u16
}

fn u13(b: &[u8], o: usize) -> u16 {
    (((b[o] & 0x1F) as u16) << 8) | b[o + 1] as u16
}

fn u12(b: &[u8], o: usize) -> usize {
    (((b[o] & 0x0F) as usize) << 8) | b[o + 1] as usize
}
