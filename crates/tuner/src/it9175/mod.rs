//! Tuner / demodulator logic of the ITE IT9175 ("SAMBA" tuner): a line-by-line port of OpenDTV's
//! `It9175Tuner.java` (itself ported from the vendor libpt275aDrv.so / libPTx1Drv.so).
//!
//! All chip access goes through [`Io`], so the logic is diff-tested (tests/diff_golden.rs) against traces of the
//! real vendor library, exactly like OpenDTV's `TunerDiffTest`.
//!
//! Flow (same order as the vendor): [`It9175::init`] after the firmware is running and LINK 0xf103 = 0x1a,
//! OFDM 0xec4c = 0xa8; then [`It9175::tune`], [`It9175::wait_lock`], [`It9175::status`].

#[rustfmt::skip]
pub mod data_pt275;
#[rustfmt::skip]
pub mod data_pt275c;
pub mod profile;

use profile::{Op, Profile};
use serde::Serialize;

pub const LINK: u8 = 0;
pub const OFDM: u8 = 8;
const BW_KHZ: u32 = 6000;
/// nv divider table (rodata 0x18360).
const NV: [u32; 9] = [48, 32, 24, 16, 12, 8, 6, 4, 2];

/// Chip register access. `proc` 0 = LINK, 8 = OFDM.
pub trait Io {
    /// Reads `n` registers, None on failure.
    fn read(&mut self, proc: u8, addr: u32, n: usize) -> Option<Vec<u8>>;
    fn write(&mut self, proc: u8, addr: u32, data: &[u8]) -> bool;
    /// Command 0x22 (firmware version); the vendor sends it as a dummy "ping".
    fn fw_version(&mut self, proc: u8) -> bool;
    fn sleep(&mut self, ms: u32);
}

/// Demodulator measurements. Fields the chip cannot supply stay None.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SignalStatus {
    pub locked: bool,
    pub strength_percent: Option<u8>,
    pub quality_percent: Option<u8>,
    /// Received power in dBm (OFDM 0x13f minus 100).
    pub dbm: Option<i32>,
    pub ber: Option<f64>,
    pub raw_strength: Option<u8>,
    pub raw_quality: Option<u8>,
    pub raw_snr: Option<u8>,
    /// Post-Viterbi error bits and measurement window (units of 204*8 bits) from OFDM 0x32..0x38.
    pub ber_errors: Option<u32>,
    pub ber_window: Option<u32>,
}

pub struct It9175 {
    profile: &'static Profile,
    /// Diagnostic log (appended, drained by the caller).
    pub log: String,
    /// LINK 0xd800 low nibble (crystal index), -1 before init.
    pub clock: i32,
    /// SAMBA tuner firmware parameters read at init.
    pub tuner_type: i32,
    pub ed03: i32,
    pub ed23: i32,
    mul_p: u64,
    mul_q: u64,
    /// Band thresholds, index = byte offset / 4 within the vendor table (1..8 used).
    thr: [u64; 9],
    init_done: bool,
    /// Result of the last 'Q' op (OFDM 0x4d != 0), consumed by 'I' ops.
    flag: bool,
    pub last_freq_khz: u32,
}

impl It9175 {
    pub fn new(profile: &'static Profile) -> Self {
        It9175 {
            profile,
            log: String::new(),
            clock: -1,
            tuner_type: 0,
            ed03: 0,
            ed23: 0,
            mul_p: 0,
            mul_q: 0,
            thr: [
                53000, 74000, 111000, 148000, 222000, 296000, 445000, 573000, 950000,
            ],
            init_done: false,
            flag: false,
            last_freq_khz: 0,
        }
    }

    pub fn profile(&self) -> &'static Profile {
        self.profile
    }

    fn note(&mut self, s: impl AsRef<str>) {
        self.log.push_str(s.as_ref());
        self.log.push('\n');
    }

    // ---------------------------------------------------------------------------------------------
    // helpers
    // ---------------------------------------------------------------------------------------------

    /// One register, -1 on failure (kept as in the Java so `& 1` tests behave identically).
    fn rd(io: &mut dyn Io, proc: u8, addr: u32) -> i32 {
        match io.read(proc, addr, 1) {
            Some(b) if !b.is_empty() => b[0] as i32,
            _ => -1,
        }
    }

    fn wr(io: &mut dyn Io, proc: u8, addr: u32, v: i32) -> bool {
        io.write(proc, addr, &[v as u8])
    }

    fn exec(&mut self, io: &mut dyn Io, op: &Op) -> bool {
        match *op {
            Op::W(p, a, d) => io.write(p, a, d),
            Op::M(p, a, mask, val) => {
                let r = Self::rd(io, p, a);
                if r < 0 {
                    return false;
                }
                Self::wr(io, p, a, (r & mask as i32) | val as i32)
            }
            Op::V(p) => io.fw_version(p),
            Op::Q(p, a) => {
                let r = Self::rd(io, p, a);
                if r < 0 {
                    return false;
                }
                self.flag = r != 0;
                true
            }
            Op::I(p, a, d) => !self.flag || io.write(p, a, d),
            Op::P(p, a) => {
                for _ in 0..150 {
                    let r = Self::rd(io, p, a);
                    if r < 0 {
                        return false;
                    }
                    if r == 0 {
                        return true;
                    }
                    io.sleep(2);
                }
                self.note(format!("poll 0x{a:x} timed out"));
                true // the vendor ignores the timeout too
            }
        }
    }

    fn run(&mut self, io: &mut dyn Io, ops: &[Op]) -> bool {
        for op in ops {
            if !self.exec(io, op) {
                self.note(format!("op failed: {op:?}"));
                return false;
            }
        }
        true
    }

    fn clock_index(&self) -> i32 {
        if self.clock == 0 {
            0
        } else {
            1
        }
    }

    // ---------------------------------------------------------------------------------------------
    // init
    // ---------------------------------------------------------------------------------------------

    pub fn init(&mut self, io: &mut dyn Io) -> bool {
        self.init_done = false;
        let d800 = Self::rd(io, LINK, 0xd800);
        if d800 < 0 {
            return false;
        }
        self.clock = d800 & 0x0F;
        self.note(format!(
            "LINK 0xd800=0x{d800:02x} -> clock index {}",
            self.clock
        ));
        if self.clock > 1 {
            self.note("clock index > 1: vendor takes an unported path, falling back to the index-1 tables");
        }
        let p = self.profile;
        if !self.run(
            io,
            if self.clock_index() == 0 {
                p.init_a0
            } else {
                p.init_a1
            },
        ) {
            return false;
        }
        if !self.samba_init(io) {
            return false;
        }
        if !self.run(
            io,
            if self.clock_index() == 0 {
                p.init_b0
            } else {
                p.init_b1
            },
        ) {
            return false;
        }
        // PIDOnOff(0) (hardware PID filter off = pass the whole TS), set_oneseg(0)
        if !self.run(
            io,
            &[
                Op::M(8, 0xf993, 0xfe, 0x00),
                Op::M(8, 0xf992, 0xfe, 0x01),
                Op::W(8, 0x1e7, &[0x00]),
            ],
        ) {
            return false;
        }
        self.init_done = true;
        true
    }

    /// samba_init: read the tuner firmware's parameters and derive the band thresholds.
    fn samba_init(&mut self, io: &mut dyn Io) -> bool {
        let t = Self::rd(io, OFDM, 0xec86);
        let d3 = Self::rd(io, OFDM, 0xed03);
        let c = io.read(OFDM, 0xed23, 2);
        let mut c = match c {
            Some(c) if t >= 0 && d3 >= 0 && c.len() >= 2 => c,
            _ => return false,
        };
        self.tuner_type = t;
        self.ed03 = d3;
        let mut v = c[0] as u32 | ((c[1] as u32) << 8);
        let (mul_p, mul_q, ed81) = match t {
            0 => (2000, 3, 0x10),
            1 => (20480, 18, 6),
            _ => (640, 1, 6),
        };
        self.mul_p = mul_p;
        self.mul_q = mul_q;
        let mut i = 1;
        while v == 0 && i <= 49 {
            io.sleep(2);
            c = match io.read(OFDM, 0xed23, 2) {
                Some(c) if c.len() >= 2 => c,
                _ => return false,
            };
            v = c[0] as u32 | ((c[1] as u32) << 8);
            i += 1;
        }
        self.ed23 = v as i32;
        self.note(format!("tuner fw: ec86={t} ed03={d3} ed23={v}"));
        if v == 0 {
            self.note("tuner firmware did not report its clock (ed23 stayed 0)");
            return false;
        }
        let nv = get_nv(d3) as u64;
        let x5 = v as u64 * self.mul_p;
        let x8 = x5 / self.mul_q;
        let x7 = ((x8 as u128 * 0xAAAA_AAAA_AAAA_AAABu128) >> 64) as u64;
        const M32: u64 = 0xFFFF_FFFF;
        self.thr[5] = (x8 >> 3) & M32;
        self.thr[3] = (x8 >> 4) & M32;
        self.thr[7] = (x5 / (nv * self.mul_q)) & M32;
        self.thr[1] = (x8 >> 5) & M32;
        self.thr[6] = (x7 >> 2) & M32;
        self.thr[4] = (x7 >> 3) & M32;
        self.thr[2] = (x7 >> 4) & M32;
        // chip 0x9175 rev 1 waits 50 ms; other chips poll OFDM 0xec82 (not ported: this chip is the target)
        let _ver = Self::rd(io, LINK, 0x1222);
        if io.read(LINK, 0x1223, 2).is_none() {
            return false;
        }
        io.sleep(50);
        if !Self::wr(io, OFDM, 0xed81, ed81) {
            return false;
        }
        let r1dc = Self::rd(io, OFDM, 0x1dc);
        if r1dc < 0 {
            return false;
        }
        if r1dc == 1
            && !self.run(
                io,
                &[Op::M(0, 0xd8d0, 0xfe, 0x01), Op::M(0, 0xd8d1, 0xfe, 0x01)],
            )
        {
            return false;
        }
        true
    }

    // ---------------------------------------------------------------------------------------------
    // frequency math (samba_get_*)
    // ---------------------------------------------------------------------------------------------

    fn get_nc(&self, f: u64) -> u32 {
        let t = &self.thr;
        if t[1] >= f {
            0
        } else if f <= t[2] {
            1
        } else if f <= t[3] {
            2
        } else if f <= t[4] {
            3
        } else if f <= t[5] {
            4
        } else if f <= t[6] {
            5
        } else if f <= t[7] {
            6
        } else if f <= t[8] {
            7
        } else {
            8
        }
    }

    /// Returns (nc, nv, fcw); code = ((nc & 7) << 13) + fcw.
    fn freq_code(&self, f: u64) -> (u64, u64, u64) {
        let nc = self.get_nc(f) as u64;
        let nv = get_nv(nc as i32) as u64;
        let prod = (f * self.mul_q) * nv;
        let den = self.mul_p;
        let mut q = prod / den;
        let rem = prod - q * den;
        if rem >= (den >> 1) {
            q += 1;
        }
        (nc, nv, q & 0xFFFF_FFFF)
    }

    fn lo_freq(&self, f: u64) -> u32 {
        let (nc, _, fcw) = self.freq_code(f);
        (((nc & 7) << 13) + fcw) as u32
    }

    fn cal_freq_iqik(&self, f: u64, p: i32) -> u32 {
        let (nc, nv, fcw) = self.freq_code(f);
        let (nc, nv, fcw) = (nc as i64, nv as i64, fcw as i64);
        let p = p as i64;
        let neg = (64 - p) & 0xFF;
        let x: i64 = if self.tuner_type == 0 {
            if p <= 0x1f {
                fcw + ((9 * nv * p) >> 5)
            } else {
                fcw - ((9 * nv * neg) >> 5)
            }
        } else if p <= 0x1f && self.tuner_type == 1 {
            fcw + ((p * nv) >> 1)
        } else {
            fcw - ((neg * nv) >> 1)
        };
        ((x + ((nc & 7) << 13)) & 0xFFFF_FFFF) as u32
    }

    fn band_row(table: &'static [[u32; 8]], f: u64) -> &'static [u32; 8] {
        let mut row = &table[0];
        for r in table {
            if r[0] as u64 <= f {
                row = r;
            } else {
                break;
            }
        }
        row
    }

    // ---------------------------------------------------------------------------------------------
    // tune
    // ---------------------------------------------------------------------------------------------

    /// SetPara for a 6 MHz channel (centre frequency in kHz).
    pub fn tune(&mut self, io: &mut dyn Io, freq_khz: u32) -> bool {
        if !self.init_done {
            self.note("tune before init");
            return false;
        }
        let f = freq_khz as u64;
        // Standard_selectBandwidth (6 MHz) + start of Standard_setFrequency
        let p = self.profile;
        if !self.run(
            io,
            if self.clock_index() == 0 {
                p.tune_pre0
            } else {
                p.tune_pre1
            },
        ) {
            return false;
        }
        let v4b = if f <= 29999 {
            0xFF
        } else if f <= 300000 {
            0
        } else if f <= 1000000 {
            1
        } else if (freq_khz.wrapping_sub(1670000)) > 10000 {
            0xFF
        } else {
            2
        };
        if !Self::wr(io, OFDM, 0x4b, v4b) {
            return false;
        }
        let r1c6 = Self::rd(io, OFDM, 0x1c6);
        let r1dc = Self::rd(io, OFDM, 0x1dc);
        if r1c6 < 0 || r1dc < 0 {
            return false;
        }
        if r1c6 != 0 {
            let r = Self::band_row(if r1dc == 1 { p.bands1 } else { p.bands0 }, f);
            let ok = Self::wr(io, OFDM, 0xfb25, r[1] as i32)
                && Self::wr(io, OFDM, 0xfbb5, r[2] as i32)
                && Self::wr(io, OFDM, 0xfbb6, r[3] as i32)
                && Self::wr(io, OFDM, 0xfbb7, r[4] as i32)
                && Self::wr(io, OFDM, 0xfbb9, r[5] as i32)
                && Self::wr(io, OFDM, 0xfbb9, r[6] as i32)
                && Self::wr(io, OFDM, 0xfbba, r[7] as i32);
            if !ok {
                return false;
            }
        }
        if !self.samba_set_freq(io, f, BW_KHZ) {
            return false;
        }
        if !Self::wr(io, OFDM, 0, 0) {
            return false;
        }
        self.last_freq_khz = freq_khz;
        true
    }

    fn samba_set_freq(&mut self, io: &mut dyn Io, f: u64, bw_khz: u32) -> bool {
        let ec4c = Self::rd(io, OFDM, 0xec4c);
        let ed81 = Self::rd(io, OFDM, 0xed81);
        if ec4c < 0 || ed81 < 0 {
            return false;
        }
        let lna = lna_cap_sel(f);
        let lpf = lpf_bw(bw_khz);
        let b5a = (ec4c & 0xE7) | if f > 0x4C2BF { 8 } else { 0 };
        let iq = self.cal_freq_iqik(f, ed81);
        let lo = self.lo_freq(f);
        let ok = Self::wr(io, OFDM, 0x160, lna)
            && Self::wr(io, OFDM, 0xec56, lpf)
            && Self::wr(io, OFDM, 0xec4c, b5a)
            && Self::wr(io, OFDM, 0xec4d, (iq & 0xFF) as i32)
            && Self::wr(io, OFDM, 0xec4e, ((iq >> 8) & 0xFF) as i32)
            && Self::wr(io, OFDM, 0x15e, (lo & 0xFF) as i32)
            && Self::wr(io, OFDM, 0x15f, ((lo >> 8) & 0xFF) as i32);
        if !ok {
            return false;
        }
        let r1dc = Self::rd(io, OFDM, 0x1dc);
        if r1dc < 0 {
            return false;
        }
        if r1dc == 1 {
            // LINK 0xd8cf bit 0 = (f > 176142 kHz)
            let r = Self::rd(io, LINK, 0xd8cf);
            if r < 0 {
                return false;
            }
            if !Self::wr(
                io,
                LINK,
                0xd8cf,
                (r & 0xFE) | if f > 0x2B00E { 1 } else { 0 },
            ) {
                return false;
            }
        }
        // samba_sethalffreq
        let half = f >> 1;
        let hl = self.lo_freq(half);
        Self::wr(io, OFDM, 0x1e3, lna_cap_sel(half))
            && Self::wr(io, OFDM, 0x1e1, (hl & 0xFF) as i32)
            && Self::wr(io, OFDM, 0x1e2, ((hl >> 8) & 0xFF) as i32)
    }

    // ---------------------------------------------------------------------------------------------
    // lock / status
    // ---------------------------------------------------------------------------------------------

    /// Standard_isLocked (single chip): 0x47 (1 = signal found, 2 = none) -> TPS lock -> MPEG2 lock.
    /// `quick` = single attempt (channel scan: an empty channel is rejected after ~1.5 s).
    pub fn wait_lock(&mut self, io: &mut dyn Io, quick: bool) -> bool {
        let attempts = if quick { 1 } else { 2 };
        for attempt in 0..attempts {
            let mut st = 0;
            for _ in 0..(if quick { 60 } else { 40 }) {
                st = Self::rd(io, OFDM, 0x47);
                if st == 1 || st == 2 || st < 0 {
                    break;
                }
                io.sleep(25);
            }
            if st == 1 {
                let mut tps = false;
                let mut i = 0;
                while i < 50 && !tps {
                    tps = (Self::rd(io, OFDM, 0xf5a9) & 1) != 0;
                    if !tps {
                        io.sleep(25);
                    }
                    i += 1;
                }
                if tps {
                    for _ in 0..40 {
                        if Self::mpeg2_locked(io) {
                            return true;
                        }
                        io.sleep(25);
                    }
                }
            }
            if st == 2 && attempt == 1 {
                break;
            }
            // vendor resets and retries once
            Self::wr(io, OFDM, 0x47, 0);
            Self::wr(io, LINK, 0, 0);
        }
        false
    }

    fn mpeg2_locked(io: &mut dyn Io) -> bool {
        let a = Self::rd(io, OFDM, 0xf9f1);
        let b = Self::rd(io, OFDM, 0xf9f2);
        let c = Self::rd(io, OFDM, 0xf9f3);
        a >= 0 && b >= 0 && c >= 0 && ((a | b | c) & 1) != 0
    }

    pub fn status(&self, io: &mut dyn Io) -> SignalStatus {
        let mut s = SignalStatus::default();
        let tps = Self::rd(io, OFDM, 0xf5a9);
        s.locked = tps >= 0 && (tps & 1) != 0 && Self::mpeg2_locked(io);
        let st = Self::rd(io, OFDM, 0x13e);
        if st >= 0 {
            s.raw_strength = Some(st as u8);
            s.strength_percent = Some(st.min(100) as u8);
        }
        let q = Self::rd(io, OFDM, 0x49);
        if q >= 0 {
            s.raw_quality = Some(q as u8);
            s.quality_percent = Some(q.min(100) as u8);
        }
        let dbm = Self::rd(io, OFDM, 0x13f);
        if dbm >= 0 {
            s.dbm = Some(dbm - 100);
        }
        let snr = Self::rd(io, OFDM, 0x1c9);
        if snr >= 0 {
            s.raw_snr = Some(snr as u8);
        }
        if let Some(b) = io.read(OFDM, 0x32, 7) {
            if b.len() >= 7 {
                let window = b[5] as u32 | ((b[6] as u32) << 8);
                let err = b[2] as u32 | ((b[3] as u32) << 8) | ((b[4] as u32) << 16);
                s.ber_errors = Some(err);
                s.ber_window = Some(window);
                if window != 0 {
                    s.ber = Some(err as f64 / (window as f64 * 204.0 * 8.0));
                }
            }
        }
        s
    }

    pub fn describe(&self) -> String {
        let t = &self.thr;
        format!(
            "clock={} tunerType={} ed03={} ed23={} thr=[{} {} {} {} {} {} {} {}]",
            self.clock,
            self.tuner_type,
            self.ed03,
            self.ed23,
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
            t[6],
            t[7],
            t[8]
        )
    }
}

pub fn get_nv(idx: i32) -> u32 {
    if (0..=8).contains(&idx) {
        NV[idx as usize]
    } else {
        2
    }
}

fn lna_cap_sel(f: u64) -> i32 {
    if f <= 0x6B6C0 {
        0
    } else if f <= 0x762A0 {
        1
    } else if f <= 0x82208 {
        2
    } else if f <= 0x8F4F8 {
        3
    } else if f <= 0x9D788 {
        4
    } else if f <= 0xAD570 {
        5
    } else if ((f.wrapping_sub(0xAD571)) & 0xFFFF_FFFF) <= 0x1193F {
        6
    } else {
        7
    }
}

fn lpf_bw(bw_khz: u32) -> i32 {
    match bw_khz {
        5000 => 0,
        5500 => 1,
        6000 => 2,
        6500 => 3,
        7000 => 4,
        7500 => 5,
        8000 => 6,
        8500 => 7,
        _ => 6,
    }
}
