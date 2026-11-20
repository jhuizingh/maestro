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

/// Collapses repeats of one failure into a count. Returns lines rather than writing them, so the
/// caller logs and tests can read them. See "Failures, staleness and logging" in DESIGN.md.
pub struct Throttle {
    window: std::time::Duration,
    runs: Mutex<std::collections::HashMap<String, Run>>,
}

struct Run {
    /// The failure as logged first, the key masked; the summary repeats it.
    what: String,
    since: chrono::DateTime<chrono::Local>,
    last_logged: std::time::Instant,
    hits: u32,
    suppressed: u32,
    /// Keys seen in this run and not yet recovered.
    failing: std::collections::BTreeSet<String>,
    all: std::collections::BTreeSet<String>,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(std::time::Duration::from_secs(15 * 60))
    }
}

impl Throttle {
    pub fn new(window: std::time::Duration) -> Self {
        Self {
            window,
            runs: Mutex::new(Default::default()),
        }
    }

    fn summary(r: &Run, tail: &str) -> String {
        let keys = r.all.len();
        format!(
            "{} — ×{} across {keys} key{} since {}{tail}",
            r.what,
            r.hits,
            if keys == 1 { "" } else { "s" },
            r.since.format("%H:%M")
        )
    }

    /// One failure `sig` on `key`. Some(line) to log: `first` the first time, a summary once the
    /// window has passed since the last line, None in between.
    pub fn hit(&self, sig: &str, what: &str, key: &str, first: String) -> Option<String> {
        let mut runs = self.runs.lock().unwrap();
        let r = runs.entry(sig.to_string()).or_insert_with(|| Run {
            what: what.to_string(),
            since: chrono::Local::now(),
            last_logged: std::time::Instant::now(),
            hits: 0,
            suppressed: 0,
            failing: Default::default(),
            all: Default::default(),
        });
        r.hits += 1;
        r.failing.insert(key.to_string());
        r.all.insert(key.to_string());
        if r.hits == 1 {
            return Some(first);
        }
        if r.last_logged.elapsed() >= self.window {
            r.last_logged = std::time::Instant::now();
            r.suppressed = 0;
            return Some(Self::summary(r, ""));
        }
        r.suppressed += 1;
        None
    }

    /// `key` succeeded. Some(line) when that ends a run with suppressed repeats in it.
    pub fn ok(&self, key: &str) -> Option<String> {
        let mut runs = self.runs.lock().unwrap();
        let mut out = vec![];
        runs.retain(|_, r| {
            if !r.failing.remove(key) || !r.failing.is_empty() {
                return true;
            }
            if r.suppressed > 0 {
                out.push(Self::summary(r, ", recovered"));
            }
            false
        });
        (!out.is_empty()).then(|| out.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn collapses_repeats_and_reports_recovery_once() {
        let t = Throttle::default();
        let hit = |k: &str| t.hit("s", "src: down", k, format!("src {k}: down"));
        assert_eq!(hit("A").as_deref(), Some("src A: down"));
        // 30 keys failing, each retried: nothing more until the window passes.
        for _ in 0..3 {
            for i in 0..30 {
                assert_eq!(hit(&format!("K{i}")), None);
            }
        }
        // Keys recover one by one; only the last closes the run, with one summary.
        for i in 0..30 {
            assert_eq!(t.ok(&format!("K{i}")), None);
        }
        let line = t.ok("A").unwrap();
        assert!(
            line.starts_with("src: down — ×91 across 31 keys since "),
            "{line}"
        );
        assert!(line.ends_with(", recovered"), "{line}");
        // A fresh failure after recovery logs its first line again.
        assert_eq!(hit("A").as_deref(), Some("src A: down"));
        // Nothing suppressed, nothing to say on recovery.
        assert_eq!(t.ok("A"), None);
        assert_eq!(t.ok("never-failed"), None);
    }

    #[test]
    fn summary_after_window() {
        let t = Throttle::new(Duration::ZERO);
        assert!(t.hit("s", "x", "A", "first".into()).is_some());
        let s = t.hit("s", "x", "A", "first".into()).unwrap();
        assert!(s.starts_with("x — ×2 across 1 key since"), "{s}");
        // Different failures are separate runs.
        assert_eq!(
            t.hit("other", "y", "A", "y A".into()).as_deref(),
            Some("y A")
        );
    }
}
