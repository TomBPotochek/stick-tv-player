//! libusb transport for the IT9175 PT275 (048d:275a) and PT275C (048d:e275) sticks: the Rust version of the desktop
//! CLI's `LibusbTuner` (same USB sequence). Chip logic stays in [`crate::it9175`].
//!
//! Endpoints: bulk OUT 0x02 commands, bulk IN 0x81 replies, bulk IN 0x84 MPEG-TS.
//!
//! Windows: the stick must be bound to WinUSB (Zadig). Linux: needs a udev rule (see README).

use crate::afa::{self, PROC_LINK, PROC_OFDM};
use crate::it9175::{profile, profile::Profile, Io, It9175, SignalStatus};
use rusb::{ffi, Context, Device, DeviceHandle, UsbContext};
use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const EP_CMD_OUT: u8 = 0x02;
pub const EP_CMD_IN: u8 = 0x81;
pub const EP_TS_IN: u8 = 0x84;

const CMD_TIMEOUT: Duration = Duration::from_millis(1000);
/// Async TS transfers: 64 KiB x 16 in flight = ~0.5 s at 17 Mbit/s.
const URB_SIZE: usize = 65536;
const URB_COUNT: usize = 16;
/// Chunks between the USB event thread and the consumer (~25 MB, ~10 s of TS).
const QUEUE_CHUNKS: usize = 400;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn err<T>(s: impl Into<String>) -> Result<T> {
    Err(Error(s.into()))
}

/// A supported stick found on the bus.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub bus: u8,
    pub address: u8,
    pub pid: u16,
    pub profile: String,
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bus {:03} addr {:03}  {:04x}:{:04x}  {}",
            self.bus,
            self.address,
            profile::VID,
            self.pid,
            self.profile
        )
    }
}

fn pid_of(d: &Device<Context>) -> Option<u16> {
    let desc = d.device_descriptor().ok()?;
    if desc.vendor_id() != profile::VID {
        return None;
    }
    profile::for_product_id(desc.product_id()).map(|_| desc.product_id())
}

/// All supported sticks currently connected.
pub fn list() -> Result<Vec<DeviceInfo>> {
    let ctx = Context::new().map_err(|e| Error(format!("libusb init failed: {e}")))?;
    let devs = ctx
        .devices()
        .map_err(|e| Error(format!("libusb device list: {e}")))?;
    Ok(devs
        .iter()
        .filter_map(|d| {
            let pid = pid_of(&d)?;
            Some(DeviceInfo {
                bus: d.bus_number(),
                address: d.address(),
                pid,
                profile: profile::for_product_id(pid).unwrap().name.to_string(),
            })
        })
        .collect())
}

/// An opened stick. Shareable across threads: commands are serialized internally, so signal status can be read while
/// another thread runs [`Stick::stream`].
pub struct Stick {
    ctx: Context,
    handle: DeviceHandle<Context>,
    pid: u16,
    profile: &'static Profile,
    claimed: Vec<u8>,
    cmd: Mutex<u8>, // sequence number; the lock serializes command/reply pairs
    tuner: Mutex<Option<It9175>>,
    log: Mutex<String>,
    ts_bytes: AtomicU64,
    dropped: AtomicU64,
}

impl Stick {
    /// Opens the `index`-th supported stick (optionally only those with product id `only_pid`) and claims the
    /// interfaces holding the three endpoints.
    pub fn open(only_pid: Option<u16>, index: usize) -> Result<Stick> {
        let ctx = Context::new().map_err(|e| Error(format!("libusb init failed: {e}")))?;
        let devs = ctx
            .devices()
            .map_err(|e| Error(format!("libusb device list: {e}")))?;
        let (dev, pid) = devs
            .iter()
            .filter_map(|d| pid_of(&d).map(|p| (d, p)))
            .filter(|(_, p)| only_pid.is_none_or(|o| o == *p))
            .nth(index)
            .ok_or_else(|| {
                Error("no PT275 / PT275C stick found (looking for 048d:275a or 048d:e275)".into())
            })?;
        let handle = dev.open().map_err(|e| {
            let hint = match e {
                rusb::Error::Access => {
                    " (Linux: install the udev rule from the README, or run as root)"
                }
                rusb::Error::NotSupported | rusb::Error::NotFound => {
                    " (Windows: bind the WinUSB driver to the stick with Zadig, see the README)"
                }
                _ => "",
            };
            Error(format!(
                "cannot open {:04x}:{pid:04x}: {e}{hint}",
                profile::VID
            ))
        })?;
        let mut s = Stick {
            ctx,
            handle,
            pid,
            profile: profile::for_product_id(pid).unwrap(),
            claimed: vec![],
            cmd: Mutex::new(0),
            tuner: Mutex::new(None),
            log: Mutex::new(String::new()),
            ts_bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        };
        s.claim(&dev)?;
        Ok(s)
    }

    fn claim(&mut self, dev: &Device<Context>) -> Result<()> {
        let cfg = dev
            .active_config_descriptor()
            .map_err(|e| Error(format!("active config descriptor: {e}")))?;
        let (mut out, mut inp, mut ts) = (false, false, false);
        let mut need: Vec<u8> = vec![];
        let mut layout = String::new();
        for itf in cfg.interfaces() {
            for alt in itf.descriptors() {
                let _ = write!(
                    layout,
                    "intf {} alt {} class 0x{:02x}:",
                    alt.interface_number(),
                    alt.setting_number(),
                    alt.class_code()
                );
                for ep in alt.endpoint_descriptors() {
                    let a = ep.address();
                    let _ = write!(layout, " 0x{a:02x}({})", ep.max_packet_size());
                    if ep.transfer_type() != rusb::TransferType::Bulk {
                        continue;
                    }
                    out |= a == EP_CMD_OUT;
                    inp |= a == EP_CMD_IN;
                    ts |= a == EP_TS_IN;
                    if (a == EP_CMD_OUT || a == EP_CMD_IN || a == EP_TS_IN)
                        && !need.contains(&alt.interface_number())
                    {
                        need.push(alt.interface_number());
                    }
                }
                layout.push('\n');
            }
        }
        self.note(layout.trim_end());
        if !(out && inp && ts) {
            return err(format!(
                "unexpected endpoint layout (cmdOut={out} cmdIn={inp} ts={ts})\n{layout}"
            ));
        }
        let _ = self.handle.set_auto_detach_kernel_driver(true); // Linux only; NotSupported elsewhere is fine
        for n in need {
            self.handle.claim_interface(n).map_err(|e| {
                let hint = if e == rusb::Error::Busy {
                    " (another program, or a kernel driver, has it)"
                } else {
                    ""
                };
                Error(format!("claim interface {n}: {e}{hint}"))
            })?;
            self.claimed.push(n);
        }
        Ok(())
    }

    /// USB port reset (the stick re-enumerates). Clears a chip left wedged by a process killed mid-stream; the
    /// handle must be dropped and the stick reopened afterwards.
    pub fn reset_device(&self) -> Result<()> {
        self.handle
            .reset()
            .map_err(|e| Error(format!("USB reset: {e}")))
    }

    pub fn product_id(&self) -> u16 {
        self.pid
    }

    pub fn profile_name(&self) -> &'static str {
        self.profile.name
    }

    /// Returns and clears the accumulated diagnostic log.
    pub fn drain_log(&self) -> String {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    fn note(&self, s: impl AsRef<str>) {
        let mut l = self.log.lock().unwrap();
        l.push_str(s.as_ref());
        l.push('\n');
    }

    // ---------------------------------------------------------------------------------------------
    // command channel
    // ---------------------------------------------------------------------------------------------

    fn transact(
        &self,
        build: impl FnOnce(u8) -> Vec<u8>,
        expect_data: usize,
        what: impl FnOnce() -> String,
    ) -> Option<Vec<u8>> {
        let mut seq = self.cmd.lock().unwrap();
        let frame = build(*seq);
        *seq = seq.wrapping_add(1);
        match self.handle.write_bulk(EP_CMD_OUT, &frame, CMD_TIMEOUT) {
            Ok(n) if n == frame.len() => {}
            Ok(n) => {
                self.note(format!(
                    "{}: write failed (sent {n} of {} bytes)",
                    what(),
                    frame.len()
                ));
                return None;
            }
            Err(e) => {
                self.note(format!("{}: write failed: {e}", what()));
                return None;
            }
        }
        let mut rx = [0u8; 512];
        let n = match self.handle.read_bulk(EP_CMD_IN, &mut rx, CMD_TIMEOUT) {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                self.note(format!("{}: empty response", what()));
                return None;
            }
            Err(e) => {
                self.note(format!("{}: no response: {e}", what()));
                return None;
            }
        };
        let data = afa::parse_response(&rx[..n], expect_data);
        if data.is_none() {
            self.note(format!(
                "{}: bad response len={n} status={:?} [{}]",
                what(),
                afa::status(&rx[..n]),
                hex(&rx[..n.min(16)])
            ));
        }
        data
    }

    pub fn read_registers(&self, proc: u8, addr: u32, count: usize) -> Option<Vec<u8>> {
        self.transact(
            |seq| afa::build_read(seq, 0, proc, addr, count as u8),
            count,
            || format!("read {proc}:0x{addr:x}"),
        )
    }

    pub fn write_registers(&self, proc: u8, addr: u32, data: &[u8]) -> bool {
        self.transact(
            |seq| afa::build_write(seq, 0, proc, addr, data),
            0,
            || format!("write {proc}:0x{addr:x}"),
        )
        .is_some()
    }

    pub fn send_command(
        &self,
        cmd: u16,
        proc: u8,
        payload: &[u8],
        reply_len: usize,
    ) -> Option<Vec<u8>> {
        self.transact(
            |seq| afa::build_command(cmd, seq, 0, proc, payload),
            reply_len,
            || format!("cmd 0x{cmd:02x}"),
        )
    }

    pub fn firmware_version(&self, proc: u8) -> Option<Vec<u8>> {
        self.send_command(0x22, proc, &[1], 4)
    }

    /// Throws away replies left in the command-IN pipe by an earlier, interrupted run.
    fn drain_replies(&self) {
        let _g = self.cmd.lock().unwrap();
        let mut rx = [0u8; 512];
        for _ in 0..16 {
            match self
                .handle
                .read_bulk(EP_CMD_IN, &mut rx, Duration::from_millis(50))
            {
                Ok(n) if n > 0 => self.note(format!("discarded stale reply ({n} bytes)")),
                _ => return,
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // bring-up (same sequence as LibusbTuner.init / UsbTunerSource.initTuner)
    // ---------------------------------------------------------------------------------------------

    /// Chip probe, firmware download if needed, post-download writes, then the IT9175 init scripts.
    pub fn init(&self) -> Result<()> {
        self.drain_replies();
        let ver = self.read_registers(PROC_LINK, 0x1222, 1);
        let id = self.read_registers(PROC_LINK, 0x1223, 2);
        let (Some(ver), Some(id)) = (ver, id) else {
            return err("the stick does not answer register reads");
        };
        self.note(format!(
            "chip version={} id=0x{:04x}, profile {}",
            ver[0],
            id[0] as u16 | ((id[1] as u16) << 8),
            self.profile.name
        ));
        let mut fw = self
            .firmware_version(PROC_LINK)
            .ok_or_else(|| Error("firmware version query failed".into()))?;
        self.note(format!("fw LINK before={}", hex(&fw)));
        if fw.iter().all(|&b| b == 0) {
            self.load_firmware()?;
            fw = self.firmware_version(PROC_LINK).unwrap_or_default();
            self.note(format!("fw LINK after={}", hex(&fw)));
            if fw.is_empty() || fw.iter().all(|&b| b == 0) {
                return err("firmware did not start");
            }
        }
        if !self.write_registers(PROC_LINK, 0xf103, &[0x1a]) {
            return err("post-download write LINK 0xf103 failed");
        }
        let mut alive = self.write_registers(PROC_OFDM, 0xec4c, &[0xa8]);
        std::thread::sleep(Duration::from_millis(10));
        if !alive {
            alive = self.ofdm_alive();
        }
        if !alive {
            self.note("OFDM silent after 0xec4c; trying the wake-up steps");
            alive = self.wake_ofdm();
        }
        if !alive {
            return err("OFDM processor does not answer: unplug the stick and plug it back in");
        }
        let fw_ofdm = self.firmware_version(PROC_OFDM);
        self.note(format!(
            "fw OFDM={}",
            fw_ofdm.as_deref().map(hex).unwrap_or_else(|| "none".into())
        ));
        let mut t = It9175::new(self.profile);
        let ok = t.init(&mut CmdIo(self));
        self.note(t.log.trim_end());
        t.log.clear();
        self.note(t.describe());
        if !ok {
            return err("tuner init failed");
        }
        *self.tuner.lock().unwrap() = Some(t);
        Ok(())
    }

    fn ofdm_alive(&self) -> bool {
        self.read_registers(PROC_OFDM, 0x46, 1).is_some()
    }

    fn set_bit0(&self, proc: u8, addr: u32) {
        if let Some(v) = self.read_registers(proc, addr, 1) {
            self.write_registers(proc, addr, &[v[0] | 1]);
        }
    }

    /// Wakes an OFDM processor that does not answer (e.g. after a process was killed mid-stream). Same steps as
    /// OpenDTV's UsbTunerSource.diagnoseOfdm: A) vendor DRV_ApCtrl: five firmware-version pings, OFDM 0xec40 = 1;
    /// B) power-saving exit: LINK 0xe00c = 0, OFDM 0xf84f = 0 then 1; C) LINK GPIO enable bits.
    fn wake_ofdm(&self) -> bool {
        for _ in 0..5 {
            self.firmware_version(PROC_LINK);
            std::thread::sleep(Duration::from_millis(1));
        }
        self.write_registers(PROC_OFDM, 0xec40, &[1]);
        std::thread::sleep(Duration::from_millis(10));
        if self.ofdm_alive() {
            self.note("OFDM woke up after ApCtrl (0xec40 = 1)");
            return true;
        }
        self.write_registers(PROC_LINK, 0xe00c, &[0]);
        self.write_registers(PROC_OFDM, 0xf84f, &[0]);
        self.write_registers(PROC_OFDM, 0xf84f, &[1]);
        std::thread::sleep(Duration::from_millis(50));
        if self.ofdm_alive() {
            self.note("OFDM woke up after power-saving exit");
            return true;
        }
        for r in [0xd8c8, 0xd8c9, 0xd8bc, 0xd8bd] {
            self.set_bit0(PROC_LINK, r);
        }
        std::thread::sleep(Duration::from_millis(50));
        if self.ofdm_alive() {
            self.note("OFDM woke up after the GPIO enable bits");
            return true;
        }
        false
    }

    /// LINK firmware version string ("3.0.4.6"), if the firmware runs.
    pub fn firmware_version_string(&self) -> Option<String> {
        let v = self.firmware_version(PROC_LINK)?;
        Some(format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]))
    }

    fn load_firmware(&self) -> Result<()> {
        if !self.write_registers(PROC_LINK, 0x4bfb, &[0])
            || !self.write_registers(PROC_LINK, 0xcfff, &[0])
            || !self.write_registers(PROC_LINK, 0xf103, &[7])
        {
            return err("pre-download register writes failed");
        }
        let image = self.profile.firmware;
        let mut off = 0;
        for (i, &n) in self.profile.fw_chunks.iter().enumerate() {
            if self
                .send_command(0x29, PROC_LINK, &image[off..off + n], 0)
                .is_none()
            {
                return err(format!("firmware chunk {i} failed"));
            }
            off += n;
        }
        if self.send_command(0x23, PROC_LINK, &[], 0).is_none() {
            return err("firmware start command failed");
        }
        std::thread::sleep(Duration::from_millis(10));
        self.note(format!("firmware downloaded ({off} bytes)"));
        Ok(())
    }

    // ---------------------------------------------------------------------------------------------
    // tune / status
    // ---------------------------------------------------------------------------------------------

    /// Tunes a 6 MHz channel (centre frequency in kHz) and waits for lock. `quick` = one short lock attempt
    /// (~1.5 s on an empty channel), for scanning. Returns whether it locked.
    pub fn tune(&self, freq_khz: u32, quick: bool) -> Result<bool> {
        let mut g = self.tuner.lock().unwrap();
        let t = g.as_mut().ok_or_else(|| Error("init() first".into()))?;
        let mut io = CmdIo(self);
        let ok = t.tune(&mut io, freq_khz);
        let log = std::mem::take(&mut t.log);
        if !log.is_empty() {
            self.note(log.trim_end());
        }
        if !ok {
            return err(format!("tune to {freq_khz} kHz failed (USB error)"));
        }
        std::thread::sleep(Duration::from_millis(50));
        Ok(t.wait_lock(&mut io, quick))
    }

    pub fn status(&self) -> Result<SignalStatus> {
        let g = self.tuner.lock().unwrap();
        let t = g.as_ref().ok_or_else(|| Error("init() first".into()))?;
        Ok(t.status(&mut CmdIo(self)))
    }

    // ---------------------------------------------------------------------------------------------
    // TS streaming (libusb async transfers on EP 0x84)
    // ---------------------------------------------------------------------------------------------

    /// Total TS bytes received since open.
    pub fn ts_bytes(&self) -> u64 {
        self.ts_bytes.load(Ordering::Relaxed)
    }

    /// USB chunks dropped because the consumer could not keep up (should stay 0).
    pub fn dropped_chunks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Streams the TS into `sink` until `keep_going` returns false or the sink returns false. USB events run on the
    /// calling thread; the sink is called from a separate thread so a slow consumer never stalls USB.
    pub fn stream(
        &self,
        sink: impl FnMut(&[u8]) -> bool + Send,
        keep_going: impl Fn() -> bool,
    ) -> Result<()> {
        let _ = self.handle.clear_halt(EP_TS_IN);
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
        let shared = Box::new(XferShared {
            tx: Mutex::new(Some(tx)),
            running: AtomicBool::new(true),
            in_flight: AtomicI32::new(0),
            errors: AtomicI32::new(0),
            ts_bytes: &self.ts_bytes as *const AtomicU64,
            dropped: &self.dropped as *const AtomicU64,
        });
        let shared_ptr = &*shared as *const XferShared as *mut c_void;

        let mut leaked = false;
        std::thread::scope(|scope| {
            let running = &shared.running;
            let writer = scope.spawn(move || {
                let mut sink = sink;
                while let Ok(c) = rx.recv() {
                    if !sink(&c) {
                        break;
                    }
                }
                running.store(false, Ordering::SeqCst);
            });

            let mut bufs: Vec<Box<[u8]>> = (0..URB_COUNT)
                .map(|_| vec![0u8; URB_SIZE].into_boxed_slice())
                .collect();
            let mut xfers: Vec<*mut ffi::libusb_transfer> = Vec::with_capacity(URB_COUNT);
            for b in bufs.iter_mut() {
                unsafe {
                    let t = ffi::libusb_alloc_transfer(0);
                    if t.is_null() {
                        break;
                    }
                    ffi::libusb_fill_bulk_transfer(
                        t,
                        self.handle.as_raw(),
                        EP_TS_IN,
                        b.as_mut_ptr(),
                        URB_SIZE as i32,
                        ts_callback,
                        shared_ptr,
                        1000,
                    );
                    xfers.push(t);
                    let r = ffi::libusb_submit_transfer(t);
                    if r != 0 {
                        self.note(format!("submit transfer: {}", rusb_err(r)));
                        break;
                    }
                    shared.in_flight.fetch_add(1, Ordering::SeqCst);
                }
            }

            let tv = libc_timeval(100_000);
            while shared.running.load(Ordering::SeqCst)
                && shared.in_flight.load(Ordering::SeqCst) > 0
                && keep_going()
            {
                unsafe {
                    ffi::libusb_handle_events_timeout_completed(
                        self.ctx.as_raw(),
                        &tv,
                        std::ptr::null_mut(),
                    );
                }
            }
            shared.running.store(false, Ordering::SeqCst);
            for &t in &xfers {
                unsafe {
                    ffi::libusb_cancel_transfer(t);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let tv = libc_timeval(50_000);
            while shared.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
                unsafe {
                    ffi::libusb_handle_events_timeout_completed(
                        self.ctx.as_raw(),
                        &tv,
                        std::ptr::null_mut(),
                    );
                }
            }
            if shared.in_flight.load(Ordering::SeqCst) == 0 {
                for &t in &xfers {
                    unsafe { ffi::libusb_free_transfer(t) };
                }
            } else {
                // A transfer still owns its buffer: leak buffers and transfers rather than risk a use-after-free.
                self.note("TS transfers did not cancel in time; leaking their buffers");
                std::mem::forget(bufs);
                leaked = true;
            }
            // Closing the channel ends the writer thread.
            shared.tx.lock().unwrap().take();
            let _ = writer.join();
        });
        let errors = shared.errors.load(Ordering::SeqCst);
        if leaked {
            Box::leak(shared); // still referenced by the uncancelled transfers
        }
        if errors >= 50 {
            return err("TS endpoint keeps failing (device unplugged?)");
        }
        Ok(())
    }
}

impl Drop for Stick {
    fn drop(&mut self) {
        for &n in &self.claimed {
            let _ = self.handle.release_interface(n);
        }
    }
}

struct XferShared {
    tx: Mutex<Option<mpsc::SyncSender<Vec<u8>>>>,
    running: AtomicBool,
    in_flight: AtomicI32,
    errors: AtomicI32,
    ts_bytes: *const AtomicU64,
    dropped: *const AtomicU64,
}

extern "system" fn ts_callback(t: *mut ffi::libusb_transfer) {
    unsafe {
        let t = &mut *t;
        let sh = &*(t.user_data as *const XferShared);
        let n = t.actual_length.max(0) as usize;
        if n > 0 {
            let c = std::slice::from_raw_parts(t.buffer, n).to_vec();
            (*sh.ts_bytes).fetch_add(n as u64, Ordering::Relaxed);
            let sent = match sh.tx.lock().unwrap().as_ref() {
                Some(tx) => tx.try_send(c).is_ok(),
                None => true,
            };
            if !sent {
                (*sh.dropped).fetch_add(1, Ordering::Relaxed);
            }
        }
        let st = t.status;
        use ffi::constants::*;
        if st != LIBUSB_TRANSFER_COMPLETED
            && st != LIBUSB_TRANSFER_TIMED_OUT
            && st != LIBUSB_TRANSFER_CANCELLED
        {
            sh.errors.fetch_add(1, Ordering::SeqCst);
        }
        if sh.running.load(Ordering::SeqCst)
            && st != LIBUSB_TRANSFER_CANCELLED
            && sh.errors.load(Ordering::SeqCst) < 50
            && ffi::libusb_submit_transfer(t) == 0
        {
            return;
        }
        sh.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn libc_timeval(us: i64) -> libc::timeval {
    libc::timeval {
        tv_sec: (us / 1_000_000) as _,
        tv_usec: (us % 1_000_000) as _,
    }
}

fn rusb_err(code: i32) -> String {
    format!("libusb error {code}")
}

/// [`Io`] over the command endpoints of a [`Stick`].
struct CmdIo<'a>(&'a Stick);

impl Io for CmdIo<'_> {
    fn read(&mut self, proc: u8, addr: u32, n: usize) -> Option<Vec<u8>> {
        self.0.read_registers(proc, addr, n)
    }

    fn write(&mut self, proc: u8, addr: u32, data: &[u8]) -> bool {
        self.0.write_registers(proc, addr, data)
    }

    fn fw_version(&mut self, proc: u8) -> bool {
        self.0.firmware_version(proc).is_some()
    }

    fn sleep(&mut self, ms: u32) {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
