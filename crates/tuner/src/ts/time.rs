//! Broadcast date and time from the TDT (table 0x70) and TOT (table 0x73) on PID 0x0014.
//!
//! The time is passed on as transmitted, to the second. The field is called UTC_time in the standard, but ISDB-Tb
//! stations send local time (Argentina: the TOT read 22:30:21 at 22:30 local time). The TOT's
//! local_time_offset_descriptor, when present, is kept as is.

use super::crc32_mpeg;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BroadcastTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 0x70 (TDT) or 0x73 (TOT).
    pub table: u8,
    /// From the TOT's local_time_offset_descriptor: minutes, negative west of Greenwich.
    pub offset_minutes: Option<i16>,
}

impl std::fmt::Display for BroadcastTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

fn bcd(b: u8) -> Option<u8> {
    let (h, l) = (b >> 4, b & 0x0F);
    (h <= 9 && l <= 9).then_some(h * 10 + l)
}

/// Modified Julian Date to (year, month, day), as in ETSI EN 300 468 annex C.
fn mjd_to_date(mjd: u16) -> (u16, u8, u8) {
    let mjd = mjd as f64;
    let yp = ((mjd - 15078.2) / 365.25).floor();
    let mp = ((mjd - 14956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let day = mjd - 14956.0 - (yp * 365.25).floor() - (mp * 30.6001).floor();
    let k = if mp == 14.0 || mp == 15.0 { 1.0 } else { 0.0 };
    (
        (1900.0 + yp + k) as u16,
        (mp - 1.0 - k * 12.0) as u8,
        day as u8,
    )
}

/// Parses a complete TDT or TOT section. None for other tables, bad CRC (TOT) or invalid digits.
pub fn parse_time_section(s: &[u8]) -> Option<BroadcastTime> {
    let table = *s.first()?;
    if (table != 0x70 && table != 0x73) || s.len() < 8 {
        return None;
    }
    if table == 0x73 && (s.len() < 14 || crc32_mpeg(s) != 0) {
        return None;
    }
    let (year, month, day) = mjd_to_date(u16::from_be_bytes([s[3], s[4]]));
    let (hour, minute, second) = (bcd(s[5])?, bcd(s[6])?, bcd(s[7])?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut offset_minutes = None;
    if table == 0x73 {
        let len = (((s[8] & 0x0F) as usize) << 8) | s[9] as usize;
        let end = (10 + len).min(s.len() - 4);
        let mut i = 10;
        while i + 2 <= end {
            let (tag, dl) = (s[i], s[i + 1] as usize);
            if tag == 0x58 && dl >= 13 && i + 2 + 13 <= end {
                // first entry: country(3) region/polarity(1) offset hhmm BCD(2) ...
                let e = &s[i + 2..];
                if let (Some(h), Some(m)) = (bcd(e[4]), bcd(e[5])) {
                    let mins = h as i16 * 60 + m as i16;
                    offset_minutes = Some(if e[3] & 0x01 != 0 { -mins } else { mins });
                }
            }
            i += 2 + dl;
        }
    }
    Some(BroadcastTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        table,
        offset_minutes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mjd_example_from_the_standard() {
        // EN 300 468 annex C: 93/10/13 12:45:00 is coded as 0xC079124500
        assert_eq!(mjd_to_date(0xC079), (1993, 10, 13));
    }

    #[test]
    fn tdt() {
        let t = parse_time_section(&[0x70, 0x70, 0x05, 0xC0, 0x79, 0x12, 0x45, 0x00]).unwrap();
        assert_eq!(t.to_string(), "1993-10-13 12:45:00");
        assert_eq!((t.table, t.offset_minutes), (0x70, None));
    }

    #[test]
    fn tot_with_offset() {
        // TOT: time, descriptors_loop_length 15, local_time_offset_descriptor ARG, -03:00
        let mut s = vec![
            0x73, 0x70, 0x00, 0xC0, 0x79, 0x23, 0x59, 0x58, 0xF0, 0x0F, 0x58, 0x0D,
        ];
        s.extend_from_slice(b"ARG");
        s.extend_from_slice(&[0x03, 0x03, 0x00, 0xC0, 0x79, 0x00, 0x00, 0x00, 0x03, 0x00]);
        let len = s.len() - 3 + 4;
        s[1] = 0x70 | (len >> 8) as u8;
        s[2] = len as u8;
        let crc = crc32_mpeg(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        let t = parse_time_section(&s).unwrap();
        assert_eq!(t.to_string(), "1993-10-13 23:59:58");
        assert_eq!(t.offset_minutes, Some(-180));
        // a flipped bit fails the CRC
        s[6] ^= 1;
        assert!(parse_time_section(&s).is_none());
    }

    #[test]
    fn rejects_bad_digits() {
        assert!(parse_time_section(&[0x70, 0x70, 0x05, 0xC0, 0x79, 0x1A, 0x45, 0x00]).is_none());
    }
}
