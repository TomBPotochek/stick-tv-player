# Stick TV Player

A small desktop app for watching ISDB-T (ISDB-Tb) TV from portable USB tuner sticks built on the
**ITE IT9175**. It scans channels, lists them and plays live TV. There's no vendor app and no JRE: it's one
Rust binary with a web UI (Tauri). Video plays in **mpv**, embedded in the window, or, without mpv, in the web
UI through an **ffmpeg** transcode (see [Players](#players-mpv-or-built-in)).

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
2. Download `Stick TV Player_<version>_x64-setup.exe` from the repository's
   [Releases](../../releases) and run it. It installs for the current user (no admin rights) and includes mpv.
   The installer isn't code-signed, so Windows SmartScreen may warn ("Windows protected your PC"): click
   *More info → Run anyway*.
3. Start **Stick TV Player**.
4. Click **Scan channels**. It tries UHF 14–51 (~1.5 s per empty channel, a few seconds per multiplex).
   The list is saved and reloaded on the next start.
5. Click a channel. Full-seg (HD/SD) and one-seg (mobile, low resolution) services are listed separately.

The top bar picks the player: **mpv** (default, bundled) or **built-in**, which needs `ffmpeg.exe` on PATH
(e.g. `winget install Gyan.FFmpeg`). See [Players](#players-mpv-or-built-in). A build without the bundled mpv (a
plain `npm run tauri build`) uses `mpv.exe` from PATH (e.g. `winget install mpv`).

Keys: **↑/↓** zap, **F** or double-click for fullscreen, **Esc** leave fullscreen, **M** mute, **I** channel info.
They work whether the focus is on the list or on the video.
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
npm run tauri dev           # development: debug build, hot reload of the UI, devtools (right-click → Inspect)
cargo test -p stick-tuner   # driver tests (no hardware needed)

# release installer with mpv bundled (what the GitHub release contains):
powershell -File tools/fetch-mpv.ps1
npm run tauri build -- --config src-tauri/tauri.mpv.conf.json
#   → target/release/bundle/nsis/Stick TV Player_<version>_x64-setup.exe  (~34 MB)
npm run tauri build         # same, without mpv (~2 MB; mpv then comes from PATH)
```

`tauri build` is always a release build: optimized, and without the webview devtools (those exist only in debug
builds unless the `devtools` Cargo feature is turned on). The frontend is built by Vite into `dist/` and embedded in
the executable, so the app needs no files next to it apart from mpv. The installer is NSIS, per-user
(`installMode: currentUser`), with a Start-menu entry and an uninstaller.

### Releasing

`.github/workflows/release.yml` does the above on GitHub's Windows runners:

1. Set `version` in `src-tauri/tauri.conf.json` (and `package.json`, `Cargo.toml` if you like them to agree), commit.
2. `git tag v0.2.0 && git push origin v0.2.0` (the tag should match that version).
3. The workflow runs the driver tests, fetches mpv, builds the installer and creates a **draft** release with it.
   Review it on GitHub and click *Publish*.

Running the workflow by hand (*Actions → Release → Run workflow*) builds the installer as a workflow artifact
without making a release. On a private repository the Windows runner minutes count double against the free quota.

The installer isn't signed. Signing needs a code-signing certificate (or Azure Trusted Signing); Tauri picks it up
from `bundle.windows.certificateThumbprint` / `signCommand`. Auto-update would be `tauri-plugin-updater` plus a
signing key for update bundles; neither is set up.

libusb is compiled from source and linked statically (`rusb` with the `vendored` feature), so no DLL is shipped.

**Bundling mpv.** `tools/fetch-mpv.ps1` downloads a pinned build from
[zhongfly/mpv-winbuild](https://github.com/zhongfly/mpv-winbuild) (checked by SHA-256) to `src-tauri/binaries/mpv/`
(gitignored), and `tauri.mpv.conf.json` installs it as `mpv\mpv.exe` next to the app. At run time the app looks for
mpv in `STICK_TV_MPV`, next to its own executable (also `mpv\mpv.exe` there), then on PATH. mpv is GPL-licensed
(source: [mpv-player/mpv](https://github.com/mpv-player/mpv)); it runs as a separate program, not linked into the app.

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

Add `-v` for the driver log (firmware download, chip ids, register failures). `stick-cli reset` does a USB
port reset.

### Troubleshooting

- **"OFDM processor does not answer"**: the chip's demodulator core is wedged, which has happened after the app
  was force-killed mid-stream. The firmware keeps running across a USB reset, and the wake-up steps (ported from
  OpenDTV) don't revive it, so unplug the stick and plug it back in.
- **Channels lock but no services are found**: the signal is too weak (quality shows a few %). Move the antenna
  and rescan.
- **"Waiting for the service…" forever**: same cause, on a channel that was found earlier. Check the quality meter.
- **Status bar says "mpv · CPU decode"**: mpv couldn't use the GPU decoder. Playback still works, with more CPU.

## How it works

```
 USB (WinUSB/libusb)            crates/tuner (Rust)                     src-tauri            webview (src/)
 ─────────────────── ┌──────────────────────────────────┐ ┌──────────────────────┐ ┌───────────────────┐
 EP 0x02 cmd OUT  ◄──┤ afa: command frames + checksum   │ │ commands: scan, play,│ │ channel list, OSD │
 EP 0x81 cmd IN   ──►│ usb: fw download, init, tune     │ │ stop, device_status  │ │ signal meters     │
                     │ it9175: tuner/demod logic        │ │                      │ │                   │
 EP 0x84 TS IN    ──►│ usb::stream: 16×64 KiB async URBs├►│ demux → ServiceFilter├►│ mpegts.js (MSE)   │
                     │ ts: PAT/PMT/SDT/NIT, filter      │ │ → 127.0.0.1/live.ts  │ │ <video>           │
                     └──────────────────────────────────┘ └──┬────────▲──────────┘ └───────────────────┘
                                                              │ mpv    │ ffmpeg (built-in player only)
                                                              ▼ (IPC)  │
                                                   mpv.exe in a child window over the video area
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
  only the chosen service (its PMT, PCR, video, audio) behind a single-program PAT, served as a chunked
  `video/mp2t` stream on `127.0.0.1`. From there, one of two players takes it (below).

## Players: mpv or built-in

**mpv** (`src-tauri/src/mpv.rs`). The app creates a native child window over the page's video area and starts one
`mpv --wid=<that window>` process, idle between channels and driven over its JSON IPC pipe (`loadfile` on each
channel change, mute, OSD text). mpv plays the service's TS untouched: GPU decode (`--hwdec=auto-safe`, d3d11va
here), deinterlacing only frames flagged interlaced (`--deinterlace=auto`). The page keeps the video area's
position in sync and hides the window while it shows a message ("Tuning…", "No signal"), so HTML is never drawn
over the video; the channel name on zapping is mpv's OSD. mpv's default key bindings are off and the app's keys are
bound to `script-message`s that come back over IPC, so ↑/↓, F, M, I and Esc work with the focus on the video.
One mpv option matters: the first pictures after tuning can't be decoded (open GOP, see below), and with the
default `--hwdec-software-fallback=3` mpv would drop to software decoding for the whole channel, so it's set to 250.
On the test machine (GTX 1660 Ti), C5N (576i) takes ~3% of one CPU core in mpv. The window is Windows-only for
now (`--wid` with an HWND); on Linux the selector offers only the built-in player.

**Built-in** (`src-tauri/src/transcode.rs`). For when mpv isn't installed: ffmpeg re-encodes the video, and
[mpegts.js](https://github.com/xqq/mpegts.js) remuxes the result to fMP4 for Media Source Extensions in the webview.
Audio (AAC-LATM) is passed through, since mpegts.js handles LATM.

### Why the built-in player re-encodes

The plan was to hand the broadcast H.264 straight to the webview, but that fails on real Argentine channels.
They send 576i/1080i as **field pictures** (PAFF), with open GOPs: the first pictures after each access point
(an I picture, never an IDR) reference pictures from before it. ffmpeg/VLC conceal that. Chromium's decoder,
which WebView2 and Edge use, aborts with `PIPELINE_ERROR_DECODE: Failed to send video packet for decoding`, and its
hardware decoder doesn't take field pictures at all. Merging field pairs into one sample and dropping leading
pictures wasn't enough (the reorder lists still point at missing pictures), so the video is re-encoded:
`yadif` deinterlaces only frames flagged interlaced, then a GPU encoder if one works, else x264. The app tries
NVENC (NVIDIA), Quick Sync (Intel), AMF (AMD) and libx264 in that order with a short test encode, and decodes with
`-hwaccel auto`. The status bar shows which encoder is in use. Set `STICK_TV_ENCODER=libx264` (or another name)
to force one. CPU time for 5.4 s of 576i on a GTX 1660 Ti machine:

| Encoder | CPU time | ≈ share of one core |
|---|---|---|
| libx264 veryfast, software decode | 1.30 s | 24% |
| h264_qsv, GPU decode | 0.61 s | 11% |
| h264_nvenc, GPU decode | 0.31 s | 6% |

In the app, during live playback, ffmpeg with NVENC took ~10% of one core. Picture appears ~4–5 s after a
channel change in both players; most of that is tuning, finding the PMT and waiting for the next access point.

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
