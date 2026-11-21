//! ffmpeg stage between the service filter and the player.
//!
//! Why not feed the broadcast H.264 to the webview directly: ISDB-Tb channels here send interlaced H.264 as field
//! pictures (PAFF), with open GOPs whose first pictures after an access point reference pictures from before it.
//! ffmpeg conceals that; Chromium's decoder (and so WebView2's MSE) aborts with "Failed to send video packet for
//! decoding", and its hardware path doesn't take field pictures at all. So the video is deinterlaced (yadif, only
//! frames flagged interlaced) and re-encoded as progressive H.264. Audio (AAC-LATM) is copied; mpegts.js handles LATM.
//!
//! The encoder is picked once per run: the first GPU encoder that works (NVENC, Quick Sync, AMF), else x264.
//! Decoding uses `-hwaccel auto` (falls back to software by itself). Measured on 5.4 s of 576i (GTX 1660 Ti):
//! x264 + software decode 1.3 s CPU, NVENC + GPU decode 0.3 s.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

/// An H.264 encoder and its low-latency settings.
#[derive(Debug, Clone)]
pub struct Encoder {
    pub name: &'static str,
    args: &'static [&'static str],
}

const CANDIDATES: &[Encoder] = &[
    Encoder {
        name: "h264_nvenc",
        args: &[
            "-preset", "p1", "-tune", "ll", "-rc", "vbr", "-cq", "21", "-b:v", "0", "-bf", "0",
        ],
    },
    Encoder {
        name: "h264_qsv",
        args: &[
            "-preset",
            "veryfast",
            "-global_quality",
            "22",
            "-pix_fmt",
            "nv12",
        ],
    },
    Encoder {
        name: "h264_amf",
        args: &[
            "-usage",
            "lowlatency",
            "-quality",
            "speed",
            "-rc",
            "cqp",
            "-qp_i",
            "22",
            "-qp_p",
            "22",
        ],
    },
    Encoder {
        name: "libx264",
        args: &["-preset", "veryfast", "-tune", "zerolatency", "-crf", "20"],
    },
];

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// The first encoder that can encode a short test clip (cached for the run). `STICK_TV_ENCODER=libx264` forces one.
pub fn encoder(ffmpeg: &PathBuf) -> &'static Encoder {
    static PICK: OnceLock<Encoder> = OnceLock::new();
    PICK.get_or_init(|| {
        let forced = std::env::var("STICK_TV_ENCODER").ok();
        for e in CANDIDATES {
            if forced.as_deref().is_some_and(|f| f != e.name) {
                continue;
            }
            let mut cmd = Command::new(ffmpeg);
            cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=720x576:rate=25:duration=0.4",
                ])
                .args(["-c:v", e.name])
                .args(e.args)
                .args(["-f", "null", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            no_window(&mut cmd);
            if cmd.status().is_ok_and(|s| s.success()) {
                return e.clone();
            }
        }
        CANDIDATES.last().unwrap().clone()
    })
}

/// Finds ffmpeg: next to the app executable first (bundled as a Tauri sidecar), then on PATH.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        let p = dir.join(exe);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

pub struct Transcoder {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Last lines of ffmpeg's stderr, for error messages.
    pub log: Arc<Mutex<String>>,
}

impl Transcoder {
    /// Starts ffmpeg reading MPEG-TS on stdin. `on_output` gets the transcoded MPEG-TS from a reader thread.
    pub fn start(
        ffmpeg: &PathBuf,
        enc: &Encoder,
        mut on_output: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
            .args([
                "-fflags",
                "+genpts+discardcorrupt",
                "-probesize",
                "1000000",
                "-analyzeduration",
                "1000000",
            ])
            .args(["-hwaccel", "auto", "-f", "mpegts", "-i", "pipe:0"])
            .args(["-map", "0:v:0", "-map", "0:a:0?"])
            .args(["-vf", "yadif=mode=0:deint=interlaced"])
            .args(["-c:v", enc.name])
            .args(enc.args)
            .args(["-g", "50"])
            .args(["-c:a", "copy"])
            .args(["-f", "mpegts", "-flush_packets", "1", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd.spawn()?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        std::thread::Builder::new()
            .name("ffmpeg-out".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    on_output(buf[..n].to_vec());
                }
            })?;
        let log2 = log.clone();
        std::thread::Builder::new()
            .name("ffmpeg-err".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut l = log2.lock().unwrap();
                    l.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if l.len() > 4000 {
                        let cut = l.len() - 2000;
                        let cut = (cut..l.len()).find(|&i| l.is_char_boundary(i)).unwrap_or(0);
                        l.drain(..cut);
                    }
                }
            })?;
        let stdin = child.stdin.take();
        Ok(Transcoder { child, stdin, log })
    }

    /// Feeds TS to ffmpeg. Returns false once ffmpeg has gone away.
    pub fn write(&mut self, data: &[u8]) -> bool {
        match self.stdin.as_mut() {
            Some(s) => s.write_all(data).is_ok(),
            None => false,
        }
    }
}

impl Drop for Transcoder {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
