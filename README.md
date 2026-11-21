# Stick TV Player

A small desktop app for watching ISDB-T (ISDB-Tb) TV from portable USB tuner sticks built on the
**ITE IT9175**. It scans channels, lists them and plays live TV. There's no vendor app and no JRE: it's one
Rust binary with a web UI (Tauri), plus **ffmpeg** for the video (see [Why ffmpeg](#why-ffmpeg)).

| Stick | USB id | Vendor driver it replaces |
|---|---|---|
| PT275C (USB-C) | `048d:e275` | `libPTx1Drv.so` (PadTV) |
| PT275 / "GMA Now" (micro-USB) | `048d:275a` | `libpt275aDrv.so` |

Tested in Argentina (UHF 14–51, 6 MHz, H.264 + AAC-LATM). Other ISDB-Tb countries using the same raster
(Brazil, Peru, Chile, …) should work as is.

Windows only for now. The code is portable, and Linux needs only a udev rule (see below) and a
`cargo tauri build` on Linux.

## Using it

1. Bind the WinUSB driver to the stick with Zadig (one time, see below).
2. Make sure `ffmpeg.exe` (with libx264, e.g. `winget install Gyan.FFmpeg`) is on PATH. Skip this if the
   installer was built with ffmpeg bundled (see Building).
3. Install and start **Stick TV Player**.
4. Click **Scan channels**. It tries UHF 14–51 (~1.5 s per empty channel, a few seconds per multiplex).
   The list is saved and reloaded on the next start.
5. Click a channel. Full-seg (HD/SD) and one-seg (mobile, low resolution) services are listed separately.

Keys: **↑/↓** zap, **F** or double-click for fullscreen, **M** mute, **I** channel info.
The status bar shows signal strength, quality, lock/dBm and the multiplex bitrate.

The app remembers the last channel and tunes it at startup if the stick is plugged in.

## Windows: bind WinUSB with Zadig (one time)

The app talks to the stick through libusb, which on Windows needs the generic **WinUSB** driver.
Windows doesn't install a driver for the stick: it shows up as "ISDB-T TV Stick" with no driver.

1. Plug in the stick and run [Zadig](https://zadig.akeo.ie/).
2. *Options → List All Devices*, then pick **ISDB-T TV Stick** (USB ID `048D E275`, or `048D 275A`).
3. Choose **WinUSB** as the target driver and click *Install Driver* (or *Replace Driver*).
4. The app's top bar should now show the stick (e.g. `PT275C (libPTx1Drv) · 048d:e275`).

To undo it, open Device Manager, uninstall the device and tick "delete the driver software". This only
affects this PC. The Android apps don't care.

## Linux (not packaged yet)

Without a rule only root can open the device. Create `/etc/udev/rules.d/60-stick-tv.rules`:

```
# ITE IT9175 ISDB-T sticks (PT275 / PT275C)
SUBSYSTEM=="usb", ATTRS{idVendor}=="048d", ATTRS{idProduct}=="e275", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="usb", ATTRS{idVendor}=="048d", ATTRS{idProduct}=="275a", MODE="0660", TAG+="uaccess"
```

Then run `sudo udevadm control --reload && sudo udevadm trigger` and replug. No kernel driver claims these ids. If one
ever does, the app detaches it while it runs. To build on Linux, add `"deb"`/`"appimage"` to `bundle.targets`
and install the [Tauri Linux prerequisites](https://tauri.app/start/prerequisites/). WebKitGTK needs
GStreamer's H.264/AAC plugins for playback.

## Building

Prerequisites (Windows): [Rust](https://rustup.rs/) (the repo pins its toolchain in `rust-toolchain.toml`,
and rustup fetches it automatically), Node.js 18+, and the MSVC build tools ("Desktop development with C++").
WebView2 ships with Windows 10/11.

```
npm install
npm run tauri build      # installer: target/release/bundle/nsis/Stick TV Player_0.1.0_x64-setup.exe
npm run tauri dev        # development, with hot reload of the UI
cargo test -p stick-tuner   # driver tests (no hardware needed)
```

libusb is compiled from source and linked statically (`rusb` with the `vendored` feature), so no DLL is shipped.

**Bundling ffmpeg (optional).** At run time the app looks for `ffmpeg.exe` next to its own executable, then on PATH.
To ship it inside the installer, run `powershell -File tools/fetch-ffmpeg.ps1`, which downloads the static
"essentials" build from gyan.dev (GPL, includes libx264) to `src-tauri/binaries/` (gitignored), and build with
`npm run tauri build -- --config src-tauri/tauri.ffmpeg.conf.json`. That adds about 30 MB to the installer.
Note the GPL terms if you redistribute such a build.

### Command-line tool

`crates/tuner` also builds `stick-cli`, which is handy for checking the stick without the GUI:

```
cargo run -p stick-tuner --bin stick-cli -- list
cargo run -p stick-tuner --bin stick-cli -- scan --from 21 --to 36
cargo run -p stick-tuner --bin stick-cli -- tune 25 -o ch25.ts --seconds 10         # whole multiplex
cargo run -p stick-tuner --bin stick-cli -- tune 25 --service E780 -o - | ffplay -  # one service
cargo run -p stick-tuner --bin stick-cli -- demux ch25.ts                           # list services in a file
```

Add `-v` for the driver log (firmware download, chip ids, register failures).

## How it works

```
 USB (WinUSB/libusb)            crates/tuner (Rust)                     src-tauri            webview (src/)
 ─────────────────── ┌──────────────────────────────────┐ ┌──────────────────────┐ ┌───────────────────┐
 EP 0x02 cmd OUT  ◄──┤ afa: command frames + checksum   │ │ commands: scan, play,│ │ channel list, OSD │
 EP 0x81 cmd IN   ──►│ usb: fw download, init, tune     │ │ stop, device_status  │ │ signal meters     │
                     │ it9175: tuner/demod logic        │ │                      │ │                   │
 EP 0x84 TS IN    ──►│ usb::stream: 16×64 KiB async URBs├►│ demux → ServiceFilter├►│ mpegts.js (MSE)   │
                     │ ts: PAT/PMT/SDT/NIT, filter      │ │ → ffmpeg → 127.0.0.1 │ │ <video>           │
                     └──────────────────────────────────┘ └──────────────────────┘ └───────────────────┘
```

- **Driver.** `crates/tuner` is a Rust port of the pure-Java IT9175 driver in
  [OpenDTV](https://github.com/jpdcs/OpenDTV) (`It9175Tuner`, `AfaCmd`, `It9175Profile`), which was in turn ported
  from the vendor's Android libraries. The USB sequence (firmware download, post-download writes, init scripts, tune,
  lock, status registers) is the same as the Java desktop CLI's.
- **Generated tables.** The firmware images, register scripts and band tables are generated from OpenDTV's Java data
  classes by `tools/gen-tables` (`sh tools/gen-tables/gen.sh [path/to/OpenDTV]`), not copied by hand.
- **Proven identical.** `crates/tuner/tests/diff_golden.rs` is a port of OpenDTV's `TunerDiffTest`. It runs the Rust
  tuner logic against traces of the real vendor libraries, run under a CPU emulator against a fake device
  (`tests/golden/tuner_golden.txt` for libpt275aDrv / PT275, `tuner_golden_ptx1.txt` for libPTx1Drv / PT275C).
  All 80 + 80 scenarios produce the identical register-write sequence.
- **Playback.** The stick sends the whole multiplex (hardware PID filter off). The backend demuxes it and keeps
  only the chosen service (its PMT, PCR, video, audio) behind a single-program PAT. ffmpeg deinterlaces and
  re-encodes the video, the result is served as a chunked `video/mp2t` stream on `127.0.0.1`, and
  [mpegts.js](https://github.com/xqq/mpegts.js) remuxes it to fMP4 for Media Source Extensions in the webview.
  Audio (AAC-LATM) is passed through, since mpegts.js handles LATM.

### Why ffmpeg

The plan was to hand the broadcast H.264 straight to the webview, but that fails on real Argentine channels.
They send 576i/1080i as **field pictures** (PAFF), with open GOPs: the first pictures after each access point
(an I picture, never an IDR) reference pictures from before it. ffmpeg/VLC conceal that. Chromium's decoder,
which WebView2 and Edge use, aborts with `PIPELINE_ERROR_DECODE: Failed to send video packet for decoding`, and its
hardware decoder doesn't take field pictures at all. Merging field pairs into one sample and dropping leading
pictures wasn't enough (the reorder lists still point at missing pictures), so the video is re-encoded.
`yadif` deinterlaces only frames flagged interlaced, then `libx264 -preset veryfast -tune zerolatency`.
SD costs a few percent of a core, 1080i about one core. Picture appears ~4–5 s after a channel change.

## ITE firmware (please read)

`crates/tuner/data/fw_pt275.bin` and `fw_pt275c.bin` are the IT9175 8051 firmware images from the vendor's Android
drivers (`libpt275aDrv.so` and `libPTx1Drv.so`). They are ITE's property and have **no redistribution licence**.
They're bundled into the app the same way OpenDTV bundles them, because the stick doesn't work without them.
The MIT licence of this repository does not cover them. If you redistribute builds, that's your call.

A cleaner option for the future is extracting the firmware at runtime from a vendor APK the user provides,
like OpenDTV's `tools/emu/gen_fw.py` does.

## Not supported (same as OpenDTV)

ALPU authentication (not used on these two sticks), crystal indexes other than 0/1, EEPROM-configured variants,
captions (ARIB B24), EPG, recording. One stick at a time.

## Licence

MIT (see `LICENSE`), including the parts derived from OpenDTV (MIT, © jpdcs). The firmware is not covered (see above).
