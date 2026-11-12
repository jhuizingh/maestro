//! Warnings and errors only, to a size-capped file. Never log per redraw.
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 1_000_000;
static LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> PathBuf {
    if let Ok(p) = std::env::var("FERMATA_LOG") {
        return PathBuf::from(p);
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fermata")
        .join("fermata.log")
}

fn write(level: &str, msg: &str) {
    if cfg!(test) {
        return;
    }
    let _g = LOCK.lock();
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = std::fs::rename(&p, p.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{} {level} {msg}", chrono::Utc::now().to_rfc3339());
    }
}

pub fn warn(msg: impl AsRef<str>) {
    write("WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write("ERROR", msg.as_ref());
}
