//! Small append-only log, `app.log` in the app data folder, for looking into playback problems after the fact
//! (channel changes, stream errors, dropped data, mpv exits). Rotated to `app.log.1` past 2 MB at startup.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("app.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2_000_000) {
        let _ = std::fs::rename(&path, dir.join("app.log.1"));
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = FILE.set(Mutex::new(f));
    }
    log(format!("--- start, version {}", env!("CARGO_PKG_VERSION")));
}

pub fn log(msg: impl AsRef<str>) {
    if let Some(f) = FILE.get() {
        let _ = writeln!(f.lock().unwrap(), "{} {}", timestamp(), msg.as_ref());
    }
}

/// UTC, e.g. `2026-11-21 03:45:19Z`.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil-from-days (Howard Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[macro_export]
macro_rules! applog {
    ($($t:tt)*) => { $crate::applog::log(format!($($t)*)) };
}
