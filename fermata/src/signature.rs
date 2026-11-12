//! A tracker directory's write signature. Measured 2026-11-12 on a real beads tracker:
//! read-only verbs bump mtimes (Dolt manifest, journal, journal.idx) and rewrite `last-touched`
//! (`get`), but leave every file size and every small file's content unchanged. A write appends
//! to the journal and moves the manifest root, so (sizes + small-file contents) changes.
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

const SMALL: u64 = 4096;

fn noise(name: &str, path: &Path) -> bool {
    name == "last-touched"
        || name == "LOCK"
        || name.ends_with(".lock")
        || path.components().any(|c| c.as_os_str() == "temptf")
}

fn walk(dir: &Path, out: &mut Vec<(String, u64, Option<Vec<u8>>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name != "temptf" {
                walk(&p, out);
            }
        } else if ft.is_file() && !noise(&name, &p) {
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            let body = (len <= SMALL).then(|| std::fs::read(&p).unwrap_or_default());
            out.push((p.to_string_lossy().into_owned(), len, body));
        }
    }
}

/// None when the directory does not exist.
pub fn signature(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    let mut v = vec![];
    walk(dir, &mut v);
    v.sort();
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    Some(format!("{:016x}-{}", h.finish(), v.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_mtime_and_noise_but_not_writes() {
        let d = std::env::temp_dir().join(format!("fermata-sig-{}", std::process::id()));
        std::fs::create_dir_all(d.join("temptf")).unwrap();
        std::fs::write(d.join("manifest"), "root-a").unwrap();
        std::fs::write(d.join("journal"), vec![0u8; 5000]).unwrap();
        let s1 = signature(&d).unwrap();
        // A "read": same bytes rewritten, last-touched and temp files change.
        std::fs::write(d.join("manifest"), "root-a").unwrap();
        std::fs::write(d.join("last-touched"), "ctx-1").unwrap();
        std::fs::write(d.join("temptf/metrics"), "x").unwrap();
        assert_eq!(signature(&d).unwrap(), s1);
        // A "write": journal grows.
        std::fs::write(d.join("journal"), vec![0u8; 5100]).unwrap();
        let s2 = signature(&d).unwrap();
        assert_ne!(s2, s1);
        std::fs::write(d.join("manifest"), "root-b").unwrap();
        assert_ne!(signature(&d).unwrap(), s2);
        std::fs::remove_dir_all(&d).unwrap();
        assert!(signature(&d).is_none());
    }
}
