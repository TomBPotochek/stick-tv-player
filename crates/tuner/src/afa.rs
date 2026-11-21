//! Command framing of the ITE/Afatech (IT9135/IT9175 family) USB command channel (port of OpenDTV's `AfaCmd`).
//!
//! Request (bulk OUT 0x02):
//! - `[0]` total length - 1 (everything after this byte, including the 2 checksum bytes)
//! - `[1]` `((chip + processor) & 0x0F) << 4`
//! - `[2]` command: 0x00 = read registers, 0x01 = write registers
//! - `[3]` sequence number
//! - `[4]` data length
//! - `[5]` register address length (1 if processor == LINK and address <= 0xFF, else 2)
//! - `[6..9]` register address, 32-bit big endian
//! - `[10..]` data (write only)
//! - last 2 bytes: checksum, big endian
//!
//! Checksum: bytes `[1 .. n-3]` summed as 16-bit big-endian words, bitwise NOT of the 16-bit sum.
//! Response (bulk IN 0x81): `[0]` length - 1, `[1]` sequence, `[2]` status (0 = OK), `[3..]` data, checksum.

pub const PROC_LINK: u8 = 0;
pub const PROC_OFDM: u8 = 8;
pub const CMD_READ_REGISTERS: u8 = 0x00;
pub const CMD_WRITE_REGISTERS: u8 = 0x01;

pub fn addr_len(processor: u8, addr: u32) -> u8 {
    if processor == PROC_LINK && addr <= 0xFF {
        1
    } else {
        2
    }
}

pub fn build_read(seq: u8, chip: u8, processor: u8, addr: u32, count: u8) -> Vec<u8> {
    build(CMD_READ_REGISTERS, seq, chip, processor, addr, count, &[])
}

pub fn build_write(seq: u8, chip: u8, processor: u8, addr: u32, data: &[u8]) -> Vec<u8> {
    build(
        CMD_WRITE_REGISTERS,
        seq,
        chip,
        processor,
        addr,
        data.len() as u8,
        data,
    )
}

/// Generic command: `[0]` len-1, `[1..2]` = cmd + ((chip + processor) << 12) big endian, `[3]` seq, `[4..]` payload,
/// checksum. Known: 0x22 = firmware version (payload {1}, 4 reply bytes), 0x23 = boot firmware, 0x29 = download chunk.
pub fn build_command(cmd: u16, seq: u8, chip: u8, processor: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; 4 + payload.len() + 2];
    let c16 = cmd.wrapping_add(((chip as u16) + (processor as u16)) << 12);
    f[1] = (c16 >> 8) as u8;
    f[2] = c16 as u8;
    f[3] = seq;
    f[4..4 + payload.len()].copy_from_slice(payload);
    finish(&mut f);
    f
}

fn build(cmd: u8, seq: u8, chip: u8, processor: u8, addr: u32, count: u8, data: &[u8]) -> Vec<u8> {
    let body = 10 + data.len();
    let mut f = vec![0u8; body + 2];
    f[1] = ((chip.wrapping_add(processor)) & 0x0F) << 4;
    f[2] = cmd;
    f[3] = seq;
    f[4] = count;
    f[5] = addr_len(processor, addr);
    f[6..10].copy_from_slice(&addr.to_be_bytes());
    f[10..10 + data.len()].copy_from_slice(data);
    finish(&mut f);
    f
}

/// Writes the length byte and the checksum into a frame whose last two bytes are reserved.
fn finish(f: &mut [u8]) {
    let n = f.len();
    let cs = checksum(f, n - 2);
    f[n - 2] = (cs >> 8) as u8;
    f[n - 1] = cs as u8;
    f[0] = (n - 1) as u8;
}

/// NOT of the 16-bit sum of bytes [1, end) taken as big-endian words.
pub fn checksum(f: &[u8], end: usize) -> u16 {
    let mut sum: u32 = 0;
    for (i, &b) in f.iter().enumerate().take(end).skip(1) {
        sum = sum.wrapping_add(if (i - 1) & 1 == 0 {
            (b as u32) << 8
        } else {
            b as u32
        });
    }
    !(sum as u16)
}

/// Validates a response frame and returns its data bytes, or None when the length, checksum or status is bad.
pub fn parse_response(r: &[u8], expect_data: usize) -> Option<Vec<u8>> {
    let len = r.len();
    if len < 5 || len != expect_data + 5 {
        return None;
    }
    if r[0] as usize != len - 1 {
        return None;
    }
    let cs = checksum(r, len - 2);
    if u16::from_be_bytes([r[len - 2], r[len - 1]]) != cs {
        return None;
    }
    if r[2] != 0 {
        return None;
    }
    Some(r[3..3 + expect_data].to_vec())
}

/// Status byte of a response, None if too short.
pub fn status(r: &[u8]) -> Option<u8> {
    r.get(2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_frame_layout() {
        let f = build_read(5, 0, PROC_OFDM, 0xf5a9, 1);
        assert_eq!(f.len(), 12);
        assert_eq!(f[0], 11);
        assert_eq!(&f[1..10], &[0x80, 0x00, 5, 1, 2, 0, 0, 0xf5, 0xa9]);
        assert_eq!(checksum(&f, 10), u16::from_be_bytes([f[10], f[11]]));
    }

    #[test]
    fn link_short_address() {
        let f = build_write(0, 0, PROC_LINK, 0x12, &[1, 2]);
        assert_eq!(f[5], 1);
        assert_eq!(f.len(), 14);
    }

    #[test]
    fn response_roundtrip() {
        // seq 7, status 0, data aa bb
        let mut r = vec![0u8, 7, 0, 0xaa, 0xbb, 0, 0];
        let cs = checksum(&r, 5);
        r[5] = (cs >> 8) as u8;
        r[6] = cs as u8;
        r[0] = 6;
        assert_eq!(parse_response(&r, 2), Some(vec![0xaa, 0xbb]));
        r[2] = 1;
        assert_eq!(parse_response(&r, 2), None);
    }

    #[test]
    fn command_frame() {
        let f = build_command(0x22, 3, 0, PROC_OFDM, &[1]);
        assert_eq!(&f[..5], &[6, 0x80, 0x22, 3, 1]);
    }
}
