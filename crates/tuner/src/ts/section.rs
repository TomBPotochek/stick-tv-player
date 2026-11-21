//! Reassembles PSI/SI sections for one PID from TS packet payloads (port of OpenDTV's SectionAssembler).
//! Handles pointer_field, sections spanning packets, several sections per packet, stuffing and CRC validation.

use super::crc32_mpeg;

pub struct SectionAssembler {
    buf: Vec<u8>,
    need: Option<usize>,
}

impl SectionAssembler {
    pub fn new() -> Self {
        SectionAssembler {
            buf: Vec::with_capacity(4096),
            need: None,
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.need = None;
    }

    /// `d` = payload of one packet. Calls `emit` for every complete, CRC-valid section.
    pub fn push(&mut self, d: &[u8], pusi: bool, emit: &mut dyn FnMut(&[u8])) {
        if pusi {
            if d.is_empty() {
                return;
            }
            let pointer = d[0] as usize;
            let d = &d[1..];
            if pointer > d.len() {
                self.reset();
                return;
            }
            // Tail of a section started in an earlier packet.
            if !self.buf.is_empty() || self.need.is_some() {
                self.consume(&d[..pointer], emit);
            }
            self.reset(); // anything still incomplete here is broken
            self.consume(&d[pointer..], emit);
        } else {
            if self.buf.is_empty() && self.need.is_none() {
                return; // no section in progress
            }
            self.consume(d, emit);
        }
    }

    fn consume(&mut self, mut d: &[u8], emit: &mut dyn FnMut(&[u8])) {
        while !d.is_empty() {
            if self.buf.is_empty() && self.need.is_none() && d[0] == 0xFF {
                return; // stuffing till packet end
            }
            let need = match self.need {
                Some(n) => n,
                None => {
                    let take = (3 - self.buf.len()).min(d.len());
                    self.buf.extend_from_slice(&d[..take]);
                    d = &d[take..];
                    if self.buf.len() < 3 {
                        return;
                    }
                    let n = 3 + ((((self.buf[1] & 0x0F) as usize) << 8) | self.buf[2] as usize);
                    if !(4..=4096).contains(&n) {
                        self.reset();
                        return;
                    }
                    self.need = Some(n);
                    n
                }
            };
            let take = (need - self.buf.len()).min(d.len());
            self.buf.extend_from_slice(&d[..take]);
            d = &d[take..];
            if self.buf.len() == need {
                let long_form = self.buf[1] & 0x80 != 0;
                if !long_form || crc32_mpeg(&self.buf) == 0 {
                    emit(&self.buf);
                }
                self.reset();
            } else {
                return;
            }
        }
    }
}
