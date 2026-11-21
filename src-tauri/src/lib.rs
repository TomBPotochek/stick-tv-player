//! Tauri backend: owns the USB stick, runs scans and playback on a worker thread, and serves the filtered TS of the
//! channel being watched over local HTTP (see `hub`).

mod hub;
mod transcode;

use hub::Hub;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stick_tuner::channels::{self, Channel, UHF_FIRST, UHF_LAST};
use stick_tuner::it9175::SignalStatus;
use stick_tuner::ts::{Demux, Packetizer, ServiceFilter};
use stick_tuner::usb::{self, Stick};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

struct Job {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct AppState {
    dev: Mutex<Option<Arc<Stick>>>,
    job: Mutex<Option<Job>>,
    hub: Arc<Hub>,
    port: u16,
    data_dir: PathBuf,
}

impl AppState {
    fn stop_job(&self) {
        let job = self.job.lock().unwrap().take();
        if let Some(j) = job {
            j.cancel.store(true, Ordering::SeqCst);
            let _ = j.handle.join();
        }
        self.hub.reset();
    }

    /// The opened and initialised stick, opening it on first use (or after it was dropped because of an error).
    fn device(&self) -> Result<Arc<Stick>, String> {
        let mut g = self.dev.lock().unwrap();
        if let Some(d) = g.as_ref() {
            return Ok(d.clone());
        }
        let s = Stick::open(None, 0).map_err(|e| e.to_string())?;
        if let Err(e) = s.init() {
            return Err(format!("{e}\n{}", s.drain_log().trim_end()));
        }
        let s = Arc::new(s);
        *g = Some(s.clone());
        Ok(s)
    }

    /// Forgets the stick (after a USB error, e.g. it was unplugged); the next use reopens it.
    fn drop_device(&self) {
        *self.dev.lock().unwrap() = None;
    }

    fn channels_file(&self) -> PathBuf {
        self.data_dir.join("channels.json")
    }
}

#[derive(Serialize, Deserialize, Default)]
struct ChannelFile {
    version: u32,
    scanned_at: u64,
    channels: Vec<Channel>,
}

#[derive(Serialize)]
struct DeviceStatus {
    devices: Vec<usb::DeviceInfo>,
    opened: Option<String>,
    firmware: Option<String>,
    error: Option<String>,
}

#[tauri::command]
fn device_status(state: State<'_, Arc<AppState>>) -> DeviceStatus {
    let opened = state.dev.lock().unwrap().clone();
    match usb::list() {
        Ok(devices) => DeviceStatus {
            devices,
            opened: opened.as_ref().map(|d| d.profile_name().to_string()),
            firmware: None,
            error: None,
        },
        Err(e) => DeviceStatus {
            devices: vec![],
            opened: None,
            firmware: None,
            error: Some(e.to_string()),
        },
    }
}

#[tauri::command]
fn load_channels(state: State<'_, Arc<AppState>>) -> ChannelFile {
    std::fs::read(state.channels_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_channels(state: &AppState, channels: &[Channel]) -> Result<(), String> {
    std::fs::create_dir_all(&state.data_dir).map_err(|e| e.to_string())?;
    let f = ChannelFile {
        version: 1,
        scanned_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        channels: channels.to_vec(),
    };
    let tmp = state.channels_file().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&f).unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state.channels_file()).map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
struct ScanDone {
    cancelled: bool,
    error: Option<String>,
    channels: Vec<Channel>,
}

#[tauri::command]
fn start_scan(app: AppHandle, state: State<'_, Arc<AppState>>, from: Option<u8>, to: Option<u8>) {
    state.stop_job();
    let st = state.inner().clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let (from, to) = (
        from.unwrap_or(UHF_FIRST).max(UHF_FIRST),
        to.unwrap_or(UHF_LAST).min(69),
    );
    let handle = std::thread::spawn(move || {
        let r = st.device().and_then(|dev| {
            channels::scan(&dev, from, to, &c2, |p| {
                let _ = app.emit("scan-progress", &p);
            })
            .map_err(|e| e.to_string())
        });
        let cancelled = c2.load(Ordering::SeqCst);
        let done = match r {
            Ok(list) => {
                let error = if cancelled {
                    None
                } else {
                    save_channels(&st, &list).err()
                };
                ScanDone {
                    cancelled,
                    error,
                    channels: list,
                }
            }
            Err(e) => {
                st.drop_device();
                ScanDone {
                    cancelled,
                    error: Some(e),
                    channels: vec![],
                }
            }
        };
        let _ = app.emit("scan-done", &done);
    });
    *state.job.lock().unwrap() = Some(Job { cancel, handle });
}

#[tauri::command]
fn stop(state: State<'_, Arc<AppState>>) {
    state.stop_job();
}

#[derive(Serialize)]
struct PlayResult {
    locked: bool,
    url: String,
    /// ffmpeg video encoder in use (h264_nvenc, h264_qsv, h264_amf or libx264).
    encoder: String,
}

#[derive(Serialize, Clone)]
struct SignalEvent {
    service_id: u16,
    signal: Option<SignalStatus>,
    mbit: f64,
    dropped: u64,
    /// The service's PMT was found and its packets are flowing to the player.
    streaming: bool,
}

#[tauri::command]
async fn play(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    freq_khz: u32,
    service_id: u16,
) -> Result<PlayResult, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || play_blocking(app, st, freq_khz, service_id))
        .await
        .map_err(|e| e.to_string())?
}

fn play_blocking(
    app: AppHandle,
    st: Arc<AppState>,
    freq_khz: u32,
    sid: u16,
) -> Result<PlayResult, String> {
    st.stop_job();
    let ffmpeg = transcode::find_ffmpeg()
        .ok_or("ffmpeg not found: it should be installed next to the app, or put ffmpeg.exe on PATH (see README)")?;
    let enc: &'static transcode::Encoder = transcode::encoder(&ffmpeg);
    // One retry with a freshly opened device: the stick may have been replugged since the last use.
    let (dev, locked) = match st.device().and_then(|d| {
        d.tune(freq_khz, false)
            .map(|l| (d, l))
            .map_err(|e| e.to_string())
    }) {
        Ok(v) => v,
        Err(_) => {
            st.drop_device();
            let d = st.device()?;
            let l = d.tune(freq_khz, false).map_err(|e| {
                st.drop_device();
                e.to_string()
            })?;
            (d, l)
        }
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let (c2, r2, hub, dev2, app2, st2) = (
        cancel.clone(),
        ready.clone(),
        st.hub.clone(),
        dev.clone(),
        app.clone(),
        st.clone(),
    );
    let handle = std::thread::spawn(move || {
        // signal / bitrate monitor
        let (c3, r3, dev3) = (c2.clone(), r2.clone(), dev2.clone());
        let monitor = std::thread::spawn(move || {
            let mut last = (dev3.ts_bytes(), Instant::now());
            while !c3.load(Ordering::SeqCst) {
                for _ in 0..10 {
                    if c3.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let b = dev3.ts_bytes();
                let mbit = (b - last.0) as f64 * 8.0 / 1e6 / last.1.elapsed().as_secs_f64();
                last = (b, Instant::now());
                let _ = app2.emit(
                    "signal",
                    SignalEvent {
                        service_id: sid,
                        signal: dev3.status().ok(),
                        mbit,
                        dropped: dev3.dropped_chunks(),
                        streaming: r3.load(Ordering::SeqCst),
                    },
                );
            }
        });
        let mut tc = match transcode::Transcoder::start(&ffmpeg, enc, move |ts| hub.broadcast(ts)) {
            Ok(t) => t,
            Err(e) => {
                c2.store(true, Ordering::SeqCst);
                let _ = monitor.join();
                let _ = app.emit(
                    "player-error",
                    format!("cannot start ffmpeg ({}): {e}", ffmpeg.display()),
                );
                return;
            }
        };
        let ff_log = tc.log.clone();
        let mut ffmpeg_died = false;
        let mut pk = Packetizer::default();
        let mut demux = Demux::new();
        let mut filter = ServiceFilter::new(sid);
        let r = dev2.stream(
            |chunk| {
                let mut out = Vec::with_capacity(chunk.len());
                pk.feed(chunk, |p| {
                    demux.packet(p);
                    filter.update(&demux);
                    filter.packet(p, &mut out);
                });
                if filter.is_ready() {
                    r2.store(true, Ordering::SeqCst);
                }
                if !out.is_empty() && !tc.write(&out) {
                    ffmpeg_died = true;
                    return false;
                }
                true
            },
            || !c2.load(Ordering::SeqCst),
        );
        let stopped = c2.swap(true, Ordering::SeqCst);
        let _ = monitor.join();
        drop(tc);
        if let Err(e) = r {
            st2.drop_device();
            let _ = app.emit("player-error", e.to_string());
        } else if ffmpeg_died && !stopped {
            let log = ff_log.lock().unwrap().trim().to_string();
            let _ = app.emit("player-error", format!("ffmpeg stopped: {log}"));
        }
    });
    *st.job.lock().unwrap() = Some(Job { cancel, handle });
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(PlayResult {
        encoder: enc.name.to_string(),
        locked,
        url: format!("http://127.0.0.1:{}/live.ts?sid={sid}&t={t}", st.port),
    })
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let hub = Arc::new(Hub::default());
            let port = hub.start()?;
            let data_dir = app.path().app_data_dir()?;
            app.manage(Arc::new(AppState {
                dev: Mutex::new(None),
                job: Mutex::new(None),
                hub,
                port,
                data_dir,
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            device_status,
            load_channels,
            start_scan,
            stop,
            play
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app");
    app.run(|handle, ev| {
        if let RunEvent::Exit = ev {
            if let Some(st) = handle.try_state::<Arc<AppState>>() {
                st.stop_job();
                st.drop_device();
            }
        }
    });
}
