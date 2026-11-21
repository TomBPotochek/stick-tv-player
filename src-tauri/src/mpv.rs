//! Playback through mpv: no re-encode. mpv decodes the broadcast H.264 itself (GPU decode, conceals the open-GOP
//! references, deinterlaces only interlaced frames) and draws into a native child window placed over the page's
//! video area. The page stays around it (channel list, meters), like a regular mpv-based player.
//!
//! One `mpv` process runs for the app's lifetime, idle between channels and driven over its JSON IPC (a named pipe on
//! Windows, a Unix socket elsewhere). It reads the filtered TS from the local hub (`/live.ts`). mpv's own key
//! bindings are off; the keys the app uses are bound to `script-message stick-key <name>` and forwarded to the page,
//! so zapping keeps working while the video window has the focus.
//!
//! The child window is Windows-only for now (`--wid` with a Win32 HWND). On other platforms [`available`] is false and
//! the page uses the ffmpeg path.

use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Key bindings while the video window has the focus: everything the page reacts to is forwarded to it.
const INPUT_CONF: &str = "\
UP script-message stick-key up
DOWN script-message stick-key down
PGUP script-message stick-key up
PGDWN script-message stick-key down
f script-message stick-key f
F script-message stick-key f
MBTN_LEFT_DBL script-message stick-key f
ESC script-message stick-key esc
m script-message stick-key m
M script-message stick-key m
i script-message stick-key i
I script-message stick-key i
";

/// What the page hears from mpv (`mpv-event`).
#[derive(Serialize, Clone)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum MpvEvent {
    /// The first frame of the loaded stream is up.
    Playing,
    /// mpv could not open or play the stream.
    Error { detail: String },
    /// The hardware decoder in use ("d3d11va", "nvdec", "no", …).
    Hwdec { value: String },
    /// A key pressed in the video window (see [`INPUT_CONF`]).
    Key { key: String },
    /// The stream ended (the connection to the hub closed); the page loads the channel again.
    Ended,
    /// mpv went away; the next play starts it again.
    Exit,
}

/// mpv executable: `STICK_TV_MPV`, next to the app, then PATH.
pub fn find_mpv() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STICK_TV_MPV").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let exe = if cfg!(windows) { "mpv.exe" } else { "mpv" };
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        for p in [dir.join(exe), dir.join("mpv").join(exe)] {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// Whether mpv playback can work here (platform with a child window, and mpv found).
pub fn available() -> Option<PathBuf> {
    if cfg!(windows) {
        find_mpv()
    } else {
        None
    }
}

struct Running {
    child: Child,
    commands: Sender<String>,
}

/// The video child window and the mpv process drawing into it.
#[derive(Default)]
pub struct MpvHost {
    /// HWND of the child window (0 until created).
    window: Mutex<isize>,
    /// Last placement from the page: x, y, w, h in physical pixels, and whether it should show.
    view: Mutex<(i32, i32, i32, i32, bool)>,
    running: Mutex<Option<Running>>,
}

impl MpvHost {
    /// Starts mpv (and creates the video window) if it isn't running.
    pub fn ensure(&self, app: &AppHandle, data_dir: &Path) -> Result<(), String> {
        let mut run = self.running.lock().unwrap();
        if let Some(r) = run.as_mut() {
            match r.child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(st)) => crate::applog!("mpv had exited: {st}"),
                Err(e) => crate::applog!("mpv status unknown: {e}"),
            }
            *run = None;
        }
        let mpv = available().ok_or(
            "mpv not found: install mpv and put it on PATH, or next to the app (see README)",
        )?;
        let wid = self.window(app)?;

        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let conf = data_dir.join("mpv-input.conf");
        std::fs::write(&conf, INPUT_CONF).map_err(|e| e.to_string())?;
        let ipc = ipc_path();
        // mpv's own log; the previous run's is kept (it has the end of the run that crashed, if one did)
        let log = data_dir.join("mpv.log");
        let _ = std::fs::rename(&log, data_dir.join("mpv.prev.log"));

        let mut cmd = Command::new(&mpv);
        cmd.arg(format!("--wid={wid}"))
            .arg(format!("--input-ipc-server={ipc}"))
            .arg(format!("--input-conf={}", conf.display()))
            .arg(format!("--log-file={}", log.display()))
            .args([
                "--no-config",
                "--idle=yes",
                "--force-window=yes",
                "--no-input-default-bindings",
                "--osc=no",
                "--cursor-autohide=1500",
                "--hwdec=auto-safe",
                // The first pictures after tuning reference ones from before (open GOP) and fail to decode; with the
                // default (3 failed frames) mpv would give up on the GPU decoder for the whole channel.
                "--hwdec-software-fallback=250",
                "--deinterlace=auto",
                "--keep-open=no",
                "--cache=no",
                "--demuxer-lavf-probesize=1000000",
                "--demuxer-lavf-analyzeduration=1",
                "--network-timeout=15",
                "--osd-font-size=40",
                "--osd-duration=3000",
                "--msg-level=all=error",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start mpv ({}): {e}", mpv.display()))?;
        crate::applog!("mpv started: {} (pid {})", mpv.display(), child.id());

        let conn = match connect(&ipc, &mut child) {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        let (tx, rx) = channel::<String>();
        let app2 = app.clone();
        std::thread::Builder::new()
            .name("mpv-ipc".into())
            .spawn(move || ipc_loop(conn, rx, app2))
            .map_err(|e| e.to_string())?;
        for c in [
            json!({ "command": ["observe_property", 1, "hwdec-current"] }),
            json!({ "command": ["disable_event", "all"] }),
            json!({ "command": ["enable_event", "playback-restart"] }),
            json!({ "command": ["enable_event", "end-file"] }),
            json!({ "command": ["enable_event", "client-message"] }),
        ] {
            let _ = tx.send(c.to_string());
        }
        *run = Some(Running {
            child,
            commands: tx,
        });
        Ok(())
    }

    fn command(&self, args: Value) {
        if let Some(r) = self.running.lock().unwrap().as_ref() {
            let _ = r.commands.send(json!({ "command": args }).to_string());
        }
    }

    pub fn load(&self, url: &str) {
        self.command(json!(["loadfile", url, "replace"]));
    }

    pub fn stop(&self) {
        self.command(json!(["stop"]));
    }

    pub fn set_mute(&self, muted: bool) {
        self.command(json!(["set_property", "mute", muted]));
    }

    pub fn show_text(&self, text: &str) {
        self.command(json!(["show-text", text, 4000]));
    }

    /// Places (or hides) the video window; `x, y, w, h` are physical pixels in the main window's client area.
    pub fn set_view(&self, x: i32, y: i32, w: i32, h: i32, visible: bool) {
        *self.view.lock().unwrap() = (x, y, w, h, visible);
        let hwnd = *self.window.lock().unwrap();
        if hwnd != 0 {
            platform::place(hwnd, x, y, w, h, visible);
        }
    }

    pub fn shutdown(&self) {
        if let Some(mut r) = self.running.lock().unwrap().take() {
            let _ = r.commands.send(json!({ "command": ["quit"] }).to_string());
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(500) {
                if matches!(r.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }

    /// The video window, created on the UI thread on first use.
    fn window(&self, app: &AppHandle) -> Result<isize, String> {
        let mut w = self.window.lock().unwrap();
        if *w != 0 {
            return Ok(*w);
        }
        let main = app.get_webview_window("main").ok_or("no main window")?;
        let parent = platform::parent_of(&main)?;
        let (tx, rx) = channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(platform::create_child(parent));
        })
        .map_err(|e| e.to_string())?;
        let hwnd = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "video window was not created")??;
        *w = hwnd;
        let (x, y, cw, ch, vis) = *self.view.lock().unwrap();
        platform::place(hwnd, x, y, cw, ch, vis);
        Ok(hwnd)
    }
}

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn ipc_path() -> String {
    let id = std::process::id();
    if cfg!(windows) {
        format!(r"\\.\pipe\stick-tv-player-mpv-{id}")
    } else {
        std::env::temp_dir()
            .join(format!("stick-tv-player-mpv-{id}.sock"))
            .display()
            .to_string()
    }
}

/// Opens the IPC endpoint once mpv has created it.
fn connect(path: &str, child: &mut Child) -> Result<platform::Conn, String> {
    let t = Instant::now();
    loop {
        match platform::Conn::open(path) {
            Ok(c) => return Ok(c),
            Err(e) => {
                if let Ok(Some(st)) = child.try_wait() {
                    return Err(format!("mpv exited at startup ({st})"));
                }
                if t.elapsed() > Duration::from_secs(5) {
                    return Err(format!("cannot connect to mpv: {e}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Owns the IPC connection: writes queued commands and reads events, from one thread. (On Windows the pipe handle is
/// synchronous, so a pending read would block writes; reads only take what `PeekNamedPipe` says is there.)
fn ipc_loop(mut conn: platform::Conn, rx: std::sync::mpsc::Receiver<String>, app: AppHandle) {
    let mut buf = vec![0u8; 16384];
    let mut pending = Vec::<u8>::new();
    loop {
        match conn.poll_read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=i).collect();
                    if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                        if v.get("event").and_then(Value::as_str) == Some("end-file") {
                            crate::applog!(
                                "mpv end-file: {} {}",
                                v.get("reason").and_then(Value::as_str).unwrap_or("?"),
                                v.get("file_error").and_then(Value::as_str).unwrap_or("")
                            );
                        }
                        if let Some(ev) = translate(&v) {
                            let _ = app.emit("mpv-event", ev);
                        }
                    }
                }
            }
            Err(_) => break,
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(mut line) => {
                line.push('\n');
                if conn.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
    crate::applog!("mpv IPC closed (mpv exited)");
    let _ = app.emit("mpv-event", MpvEvent::Exit);
}

fn translate(v: &Value) -> Option<MpvEvent> {
    match v.get("event")?.as_str()? {
        "playback-restart" => Some(MpvEvent::Playing),
        "end-file" if v.get("reason").and_then(Value::as_str) == Some("eof") => {
            Some(MpvEvent::Ended)
        }
        "end-file" if v.get("reason").and_then(Value::as_str) == Some("error") => {
            Some(MpvEvent::Error {
                detail: v
                    .get("file_error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            })
        }
        // null while idle: nothing to report
        "property-change" if v.get("name").and_then(Value::as_str) == Some("hwdec-current") => v
            .get("data")
            .and_then(Value::as_str)
            .map(|d| MpvEvent::Hwdec {
                value: d.to_string(),
            }),
        "client-message" => {
            let args = v.get("args")?.as_array()?;
            (args.first()?.as_str()? == "stick-key").then(|| MpvEvent::Key {
                key: args
                    .get(1)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;
    use std::sync::Once;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, LoadCursorW, RegisterClassW, SetWindowPos, HWND_TOP,
        IDC_ARROW, SWP_ASYNCWINDOWPOS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_SHOWWINDOW, WNDCLASSW,
        WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
    };

    pub struct Conn(File);

    impl Conn {
        pub fn open(path: &str) -> std::io::Result<Self> {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map(Conn)
        }

        /// Reads what is already in the pipe, without blocking. Ok(0) = nothing there.
        pub fn poll_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut avail = 0u32;
            let ok = unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut avail,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if avail == 0 {
                return Ok(0);
            }
            let n = (avail as usize).min(buf.len());
            self.0.read(&mut buf[..n])
        }

        pub fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
            self.0.write_all(b)
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn parent_of(w: &tauri::WebviewWindow) -> Result<isize, String> {
        w.hwnd().map(|h| h.0 as isize).map_err(|e| e.to_string())
    }

    /// A black child window of `parent`, hidden; mpv embeds its own window in it.
    pub fn create_child(parent: isize) -> Result<isize, String> {
        static REGISTER: Once = Once::new();
        let class = wide("StickTvVideo");
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            REGISTER.call_once(|| {
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(DefWindowProcW),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: std::ptr::null_mut(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    hbrBackground: GetStockObject(BLACK_BRUSH),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: class.as_ptr(),
                };
                RegisterClassW(&wc);
            });
            let h = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                16,
                16,
                parent as HWND,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if h.is_null() {
                Err(format!(
                    "cannot create the video window: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(h as isize)
            }
        }
    }

    /// Moves the window on top of its siblings (the webview) and shows or hides it. Asynchronous: callable from any
    /// thread without waiting on the UI thread.
    pub fn place(hwnd: isize, x: i32, y: i32, w: i32, h: i32, visible: bool) {
        let show = if visible && w > 0 && h > 0 {
            SWP_SHOWWINDOW
        } else {
            SWP_HIDEWINDOW
        };
        unsafe {
            SetWindowPos(
                hwnd as HWND,
                HWND_TOP,
                x,
                y,
                w.max(1),
                h.max(1),
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | show,
            );
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;

    pub struct Conn(UnixStream);

    impl Conn {
        pub fn open(path: &str) -> std::io::Result<Self> {
            let s = UnixStream::connect(path)?;
            s.set_nonblocking(true)?;
            Ok(Conn(s))
        }

        pub fn poll_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buf) {
                Ok(0) => Err(ErrorKind::UnexpectedEof.into()),
                Ok(n) => Ok(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }

        pub fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
            self.0.set_nonblocking(false)?;
            let r = self.0.write_all(b);
            self.0.set_nonblocking(true)?;
            r
        }
    }

    pub fn parent_of(_: &tauri::WebviewWindow) -> Result<isize, String> {
        Err("mpv playback needs a native child window; only Windows is supported so far".into())
    }

    pub fn create_child(_: isize) -> Result<isize, String> {
        Err("not supported on this platform".into())
    }

    pub fn place(_: isize, _: i32, _: i32, _: i32, _: i32, _: bool) {}
}
