//! Diagnostic log (`diagnostica.log` next to the config): what the engine
//! did and when, to explain problems like outputs drifting apart.
//! Never called from the audio callbacks.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn path() -> PathBuf {
    crate::config::Config::path().with_file_name("diagnostica.log")
}

pub fn init() {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&p).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&p, p.with_extension("log.old"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        if let Ok(mut g) = LOG.lock() {
            *g = Some(f);
        }
    }
    log(format!("=== MultiAudio {} avviato ({}) ===", env!("CARGO_PKG_VERSION"), std::env::consts::OS));
}

fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn log(msg: impl AsRef<str>) {
    let line = format!("{} {}\n", timestamp(), msg.as_ref());
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}
