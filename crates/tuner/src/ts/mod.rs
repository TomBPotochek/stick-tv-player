//! MPEG-TS handling for the player: PSI/SI demux (PAT, PMT, SDT, NIT) and a single-service filter.
//! Ported in spirit from OpenDTV's `ph.opendtv.ts` (TsDemux, SectionAssembler, ServiceFilter, Crc32Mpeg).

mod demux;
mod filter;
mod section;
mod text;
mod time;

pub use demux::{Demux, Service, Stream};
pub use filter::ServiceFilter;
pub use text::decode_text;
pub use time::{parse_time_section, BroadcastTime};

pub const PKT: usize = 188;
pub const PID_PAT: u16 = 0x0000;
pub const PID_NIT: u16 = 0x0010;
pub const PID_SDT: u16 = 0x0011;
pub const PID_TDT: u16 = 0x0014;
pub const PID_NULL: u16 = 0x1FFF;

/// CRC-32/MPEG-2 as used by PSI/SI long sections. A section including its CRC field yields 0.
pub fn crc32_mpeg(b: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = (i as u32) << 24;
            for _ in 0..8 {
                c = if c & 0x8000_0000 != 0 {
                    (c << 1) ^ 0x04C1_1DB7
                } else {
                    c << 1
                };
            }
            *e = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &x in b {
        crc = (crc << 8) ^ t[((crc >> 24) ^ x as u32) as usize & 0xFF];
    }
    crc
}

/// PID of a TS packet.
pub fn pid(p: &[u8]) -> u16 {
    (((p[1] & 0x1F) as u16) << 8) | p[2] as u16
}

/// Splits a byte stream into 188-byte packets aligned on the 0x47 sync byte (handles packets split across reads).
#[derive(Default)]
pub struct Packetizer {
    carry: Vec<u8>,
    pub sync_losses: u64,
}

impl Packetizer {
    pub fn feed(&mut self, data: &[u8], mut on_packet: impl FnMut(&[u8])) {
        // Complete a packet split across reads first, so the common path works on `data` without copying.
        let mut b = data;
        if !self.carry.is_empty() {
            let take = (PKT - self.carry.len()).min(b.len());
            self.carry.extend_from_slice(&b[..take]);
            if self.carry.len() < PKT {
                return;
            }
            if self.carry[0] == 0x47 && (b.len() == take || b[take] == 0x47) {
                on_packet(&self.carry);
                b = &b[take..];
                self.carry.clear();
            } else {
                // Misaligned: rescan carry + data together.
                self.sync_losses += 1;
                let mut joined = std::mem::take(&mut self.carry);
                joined.truncate(PKT - take);
                joined.extend_from_slice(b);
                return self.scan(&joined, &mut on_packet);
            }
        }
        self.scan(b, &mut on_packet);
    }

    fn scan(&mut self, b: &[u8], on_packet: &mut impl FnMut(&[u8])) {
        let mut p = 0;
        loop {
            // find sync: a 0x47 followed by another one PKT later (or the end of the data)
            while p < b.len() && !(b[p] == 0x47 && (p + PKT >= b.len() || b[p + PKT] == 0x47)) {
                p += 1;
            }
            if b.len() - p < PKT {
                break;
            }
            on_packet(&b[p..p + PKT]);
            p += PKT;
            if p < b.len() && b[p] != 0x47 {
                self.sync_losses += 1;
            }
        }
        self.carry.clear();
        self.carry.extend_from_slice(&b[p..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_of_section_with_crc_is_zero() {
        // PAT: ts_id 1, program 1 -> PMT 0x100
        let mut s = vec![0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xE1, 0x00];
        let c = crc32_mpeg(&s);
        s.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc32_mpeg(&s), 0);
    }

    #[test]
    fn packetizer_handles_split_and_garbage() {
        let mut stream = vec![0x12, 0x34];
        for i in 0..3u8 {
            let mut p = vec![0xFFu8; PKT];
            p[0] = 0x47;
            p[3] = i;
            stream.extend_from_slice(&p);
        }
        let mut pk = Packetizer::default();
        let mut got = vec![];
        for chunk in stream.chunks(100) {
            pk.feed(chunk, |p| got.push(p[3]));
        }
        assert_eq!(got, vec![0, 1, 2]);
    }
}
