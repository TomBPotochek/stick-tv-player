//! SI text decoding for ISDB-Tb (ABNT NBR 15603-2): strings are ARIB STD-B24 style 8-bit text whose usual sets in
//! South America are ASCII in GL and ISO/IEC 8859-15 (Latin-9) in GR. Control codes and escape sequences are
//! dropped; 2-byte JIS sets (Japanese) are not decoded.

pub fn decode_text(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            0x1B => {
                // ESC: skip intermediates (0x24..0x2F) and one final byte
                i += 1;
                while i < b.len() && (0x24..=0x2F).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            0x20..=0x7E => s.push(c as char),
            0xA0..=0xFF => s.push(latin9(c)),
            0x0D | 0x0A => s.push(' '),
            _ => {} // C0/C1 controls (APR, LS0/LS1, SS2/SS3 shifts, colour codes...)
        }
        i += 1;
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// ISO/IEC 8859-15 differs from Latin-1 in eight code points.
fn latin9(c: u8) -> char {
    match c {
        0xA4 => '€',
        0xA6 => 'Š',
        0xA8 => 'š',
        0xB4 => 'Ž',
        0xB8 => 'ž',
        0xBC => 'Œ',
        0xBD => 'œ',
        0xBE => 'Ÿ',
        _ => c as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_and_controls() {
        assert_eq!(decode_text(b"\x1b\x28\x42Am\xe9rica  TV\x0e"), "América TV");
        assert_eq!(decode_text(b"Ca\xf1a \xa4"), "Caña €");
    }
}
