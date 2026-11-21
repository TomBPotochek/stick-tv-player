//! Per-dongle data for the IT9175 driver (Rust side of OpenDTV's `It9175Profile`).
//!
//! The vendor app ships two builds of the same ITE SDK and picks one by USB product id: 048d:275a / 275b use
//! libpt275aDrv.so, 048d:e275 (PT275C) uses libPTx1Drv.so. The tune code is the same but the firmware image and the
//! init register scripts differ. The tables are generated (tools/gen-tables) from OpenDTV's Java data classes.

/// One register-script operation. `p` = processor: 0 = LINK, 8 = OFDM.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    /// Write bytes.
    W(u8, u32, &'static [u8]),
    /// Read-modify-write: `(r & mask) | val`.
    M(u8, u32, u8, u8),
    /// Firmware version query (command 0x22), used by the vendor as a "ping".
    V(u8),
    /// Read a register and remember whether it is non-zero (consumed by `I`).
    Q(u8, u32),
    /// Write bytes only if the last `Q` saw a non-zero value.
    I(u8, u32, &'static [u8]),
    /// Poll until the register reads 0 (150 x 2 ms; a timeout is ignored like the vendor does).
    P(u8, u32),
}

pub struct Profile {
    pub name: &'static str,
    pub firmware: &'static [u8],
    /// Firmware chunk sizes in download order (each sent with command 0x29).
    pub fw_chunks: &'static [usize],
    pub init_a0: &'static [Op],
    pub init_b0: &'static [Op],
    pub tune_pre0: &'static [Op],
    pub init_a1: &'static [Op],
    pub init_b1: &'static [Op],
    pub tune_pre1: &'static [Op],
    /// Band rows for OFDM 0x1dc = 0 / 1: startKHz, fb25, fbb5, fbb6, fbb7, fbb9a, fbb9b, fbba.
    pub bands0: &'static [[u32; 8]],
    pub bands1: &'static [[u32; 8]],
}

pub const VID: u16 = 0x048d;
pub const PID_PT275: u16 = 0x275a;
pub const PID_PT275C: u16 = 0xe275;

/// Profile for a USB product id (vendor 0x048d), or None if the dongle is not supported.
pub fn for_product_id(pid: u16) -> Option<&'static Profile> {
    match pid {
        PID_PT275 => Some(&super::data_pt275::PT275),
        PID_PT275C => Some(&super::data_pt275c::PT275C),
        _ => None,
    }
}
