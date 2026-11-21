//! Userspace driver for the ITE IT9175 ISDB-T USB sticks sold as MyGica/Geniatech PT275 (048d:275a) and PT275C
//! (048d:e275), plus the bits of MPEG-TS handling a TV player needs (PSI/SI demux, single-service filter).
//!
//! - [`afa`]: command framing on the bulk command endpoints.
//! - [`it9175`]: tuner/demodulator logic (port of OpenDTV's `It9175Tuner`), diff-tested against vendor traces.
//! - [`usb`]: libusb transport (rusb): firmware download, chip bring-up, async TS streaming.
//! - [`ts`]: PAT/PMT/SDT/NIT demux and the service filter.
//! - [`channels`]: the ISDB-Tb UHF raster and the scan routine.

pub mod afa;
pub mod channels;
pub mod it9175;
pub mod ts;
pub mod usb;
