//! A tracker directory's write signature. Measured 2026-11-12 on a real beads tracker:
//! read-only verbs bump mtimes (Dolt manifest, journal, journal.idx) and rewrite `last-touched`
//! (`get`), but leave every file size and every small file's content unchanged. A write appends
//! to the journal and moves the manifest root, so (sizes + small-file contents) changes.
//!
//! The signature depends only on what is on disk. `Signer` gets it without reading everything
//! each time: it stats every file, and lists a directory or reads a small file again only when
//! its stamp moved, its stamp is too young to trust, or a watch event named it.
use crate::util::Stamp;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SMALL: u64 = 4096;
/// How often everything is listed and read again, whatever the stamps say.
const FULL_EVERY: Duration = Duration::from_secs(60);

/// Whether a path (relative to the tracker directory) never counts towards the signature.
pub fn noise(rel: &Path) -> bool {
    let name = rel.file_name().unwrap_or_default().to_string_lossy();
    name == "last-touched"
        || name == "LOCK"
        || name.ends_with(".lock")
        || rel.components().any(|c| c.as_os_str() == "temptf")
}

/// What a watch event says may have changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Touched {
    Paths(Vec<PathBuf>),
    /// Events were dropped, or named nothing: read everything.
    Unknown,
}

struct File {
    stamp: Stamp,
    /// A later stat with the same stamp proves the content is the same.
    settled: bool,
    /// Hash of a small file's content.
    body: Option<u64>,
}

#[derive(Default)]
struct Dir {
    stamp: Option<Stamp>,
    settled: bool,
    files: BTreeMap<OsString, File>,
    dirs: BTreeMap<OsString, Dir>,
}

/// One pass over a tree: what to re-read although its stamp has not moved.
struct Pass<'a> {
    full: bool,
    named: &'a HashSet<PathBuf>,
    /// Files and directories whose content was read, for tests.
    reads: usize,
}

impl Pass<'_> {
    fn must(&self, p: &Path) -> bool {
        self.full || self.named.contains(p)
    }
}

fn read_body(p: &Path, len: u64) -> (Option<u64>, bool) {
    if len > SMALL {
        return (None, true);
    }
    let (bytes, ok) = match std::fs::read(p) {
        Ok(b) => (b, true),
        Err(_) => (vec![], false),
    };
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    (Some(h.finish()), ok)
}

impl File {
    /// None when it is gone or no longer a regular file.
    fn check(old: Option<File>, p: &Path, pass: &mut Pass) -> Option<File> {
        let stamp = Stamp::of(&std::fs::symlink_metadata(p).ok().filter(|m| m.is_file())?);
        if let Some(f) = old
            && f.settled
            && f.stamp == stamp
            && !pass.must(p)
        {
            return Some(f);
        }
        let settled = stamp.settled();
        let (body, ok) = read_body(p, stamp.len);
        pass.reads += usize::from(body.is_some());
        Some(File {
            stamp,
            settled: settled && ok,
            body,
        })
    }
}

impl Dir {
    /// False when the directory is gone.
    fn check(&mut self, p: &Path, pass: &mut Pass) -> bool {
        let Some(m) = std::fs::symlink_metadata(p).ok().filter(|m| m.is_dir()) else {
            return false;
        };
        let stamp = Stamp::of(&m);
        if !self.settled || self.stamp != Some(stamp) || pass.must(p) {
            self.settled = stamp.settled();
            self.stamp = Some(stamp);
            pass.reads += 1;
            self.list(p, pass);
        } else {
            let files = std::mem::take(&mut self.files);
            for (n, f) in files {
                if let Some(f) = File::check(Some(f), &p.join(&n), pass) {
                    self.files.insert(n, f);
                }
            }
        }
        self.dirs.retain(|n, d| d.check(&p.join(n), pass));
        true
    }

    /// Match `files` and `dirs` to the directory's entries. Checks the files, not the dirs.
    fn list(&mut self, p: &Path, pass: &mut Pass) {
        let mut files = std::mem::take(&mut self.files);
        let mut dirs = std::mem::take(&mut self.dirs);
        let Ok(rd) = std::fs::read_dir(p) else {
            self.settled = false;
            return;
        };
        for e in rd {
            let Ok(e) = e else {
                self.settled = false;
                continue;
            };
            let name = e.file_name();
            let Ok(ft) = e.file_type() else {
                self.settled = false;
                continue;
            };
            if ft.is_dir() {
                if name != "temptf" {
                    let d = dirs.remove(&name).unwrap_or_default();
                    self.dirs.insert(name, d);
                }
            } else if ft.is_file()
                && !noise(Path::new(&name))
                && let Some(f) = File::check(files.remove(&name), &p.join(&name), pass)
            {
                self.files.insert(name, f);
            }
        }
    }

    fn hash(&self, rel: &Path, h: &mut DefaultHasher) -> usize {
        for (n, f) in &self.files {
            (rel.join(n), f.stamp.len, f.body).hash(h);
        }
        self.files.len()
            + self
                .dirs
                .iter()
                .map(|(n, d)| d.hash(&rel.join(n), h))
                .sum::<usize>()
    }
}

#[derive(Default)]
struct Tree {
    /// Canonical, as watch events name it.
    root: PathBuf,
    top: Dir,
    last_full: Option<Instant>,
    reads: usize,
}

#[derive(Default)]
struct Marks {
    paths: HashSet<PathBuf>,
    unknown: bool,
}

/// The write signatures of tracker directories, remembering what it has read.
#[derive(Default)]
pub struct Signer {
    trees: Mutex<HashMap<PathBuf, Arc<Mutex<Tree>>>>,
    /// What watch events named since the last signature, by directory.
    marks: Mutex<HashMap<PathBuf, Marks>>,
}

impl Signer {
    /// A watch event in `dir`: the next signature reads these again, whatever their stamps say.
    pub fn mark(&self, dir: &Path, t: &Touched) {
        let mut all = self.marks.lock().unwrap();
        let m = all.entry(dir.to_path_buf()).or_default();
        match t {
            Touched::Paths(ps) => m.paths.extend(ps.iter().cloned()),
            Touched::Unknown => m.unknown = true,
        }
    }

    /// None when the directory does not exist.
    pub fn signature(&self, dir: &Path) -> Option<String> {
        let root = dir.canonicalize().ok().filter(|r| r.is_dir())?;
        let tree = self.tree(dir);
        let mut t = tree.lock().unwrap();
        // Marks are taken before anything is read: a later write marks again.
        let marks = self.marks.lock().unwrap().remove(dir).unwrap_or_default();
        if t.root != root {
            *t = Tree {
                root: root.clone(),
                ..Default::default()
            };
        }
        let full = marks.unknown
            || t.last_full.is_none_or(|l| l.elapsed() >= FULL_EVERY)
            || marks.paths.iter().any(|p| !p.starts_with(&root));
        if full {
            t.last_full = Some(Instant::now());
        }
        // A named path may be new or gone, so its directory is listed again too.
        let parents = marks.paths.iter().filter_map(|p| p.parent());
        let parents: Vec<PathBuf> = parents.map(Path::to_path_buf).collect();
        let mut named = marks.paths;
        named.extend(parents);
        let mut pass = Pass {
            full,
            named: &named,
            reads: 0,
        };
        if !t.top.check(&root, &mut pass) {
            return None;
        }
        t.reads = pass.reads;
        let mut h = DefaultHasher::new();
        let n = t.top.hash(Path::new(""), &mut h);
        Some(format!("{:016x}-{n}", h.finish()))
    }

    fn tree(&self, dir: &Path) -> Arc<Mutex<Tree>> {
        self.trees
            .lock()
            .unwrap()
            .entry(dir.to_path_buf())
            .or_default()
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct Tmp(PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A tracker-like directory whose stamps are old enough to trust.
    fn tracker(name: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("fermata-sig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("temptf")).unwrap();
        std::fs::create_dir_all(d.join("noms/deep")).unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("noms/deep/f{i}")), "same").unwrap();
        }
        std::fs::write(d.join("noms/manifest"), "root-a").unwrap();
        std::fs::write(d.join("noms/journal"), vec![0u8; 5000]).unwrap();
        Tmp(d.canonicalize().unwrap())
    }

    /// Wait until every stamp written so far counts as settled.
    fn settle() {
        std::thread::sleep(crate::util::SETTLE + Duration::from_millis(50));
    }

    fn reads(s: &Signer, d: &Path) -> usize {
        s.tree(d).lock().unwrap().reads
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
    }

    /// The remembered signature must always be what a fresh reader computes.
    fn check(s: &Signer, d: &Path) -> String {
        let got = s.signature(d).unwrap();
        assert_eq!(got, Signer::default().signature(d).unwrap());
        got
    }

    #[test]
    fn ignores_mtime_and_noise_but_not_writes() {
        let t = tracker("noise");
        let (d, s) = (&t.0, Signer::default());
        let s1 = check(&s, d);
        // A "read": same bytes rewritten, last-touched, locks and temp files change.
        std::fs::write(d.join("noms/manifest"), "root-a").unwrap();
        std::fs::write(d.join("last-touched"), "ctx-1").unwrap();
        std::fs::write(d.join("noms/LOCK"), "1").unwrap();
        std::fs::write(d.join("x.lock"), "1").unwrap();
        std::fs::write(d.join("temptf/metrics"), "x").unwrap();
        assert_eq!(check(&s, d), s1);
        // A "write": journal grows.
        std::fs::write(d.join("noms/journal"), vec![0u8; 5100]).unwrap();
        let s2 = check(&s, d);
        assert_ne!(s2, s1);
        std::fs::write(d.join("noms/manifest"), "root-b").unwrap();
        assert_ne!(check(&s, d), s2);
        std::fs::remove_dir_all(d).unwrap();
        assert!(s.signature(d).is_none());
    }

    #[test]
    fn notices_every_kind_of_change_without_events() {
        let t = tracker("kinds");
        let (d, s) = (&t.0, Signer::default());
        let mut last = check(&s, d);
        settle();
        check(&s, d);
        let mut differs = |what: &str| {
            let now = check(&s, d);
            assert_ne!(now, last, "{what}");
            last = now;
        };
        std::fs::write(d.join("noms/manifest"), "root-b").unwrap();
        differs("same size, new content");
        std::fs::write(d.join("noms/deep/new"), "n").unwrap();
        differs("a new file");
        std::fs::create_dir_all(d.join("noms/a/b")).unwrap();
        std::fs::write(d.join("noms/a/b/c"), "n").unwrap();
        differs("a file in new directories");
        std::fs::rename(d.join("noms/a"), d.join("noms/z")).unwrap();
        differs("a renamed directory");
        std::fs::write(d.join("noms/tmp"), "root-c").unwrap();
        std::fs::rename(d.join("noms/tmp"), d.join("noms/manifest")).unwrap();
        differs("replaced by rename");
        std::fs::remove_file(d.join("noms/deep/f3")).unwrap();
        differs("a removed file");
        std::fs::remove_dir_all(d.join("noms/deep")).unwrap();
        differs("a removed directory");
        std::fs::write(d.join("noms/z/b/c"), "").unwrap();
        differs("truncated");
    }

    #[test]
    fn a_write_that_keeps_size_and_mtime_is_noticed() {
        let t = tracker("times");
        let (d, s) = (&t.0, Signer::default());
        let m = d.join("noms/manifest");
        settle();
        let s1 = check(&s, d);
        let at = std::fs::metadata(&m).unwrap().modified().unwrap();
        std::fs::write(&m, "root-b").unwrap();
        set_mtime(&m, at);
        assert_eq!(std::fs::metadata(&m).unwrap().modified().unwrap(), at);
        assert_ne!(check(&s, d), s1);
    }

    #[test]
    fn a_young_stamp_is_not_trusted() {
        let t = tracker("young");
        let (d, s) = (&t.0, Signer::default());
        let m = d.join("noms/manifest");
        let s1 = check(&s, d);
        // As if the filesystem's clock were too coarse to tell this write from the read.
        std::fs::write(&m, "root-b").unwrap();
        s.forge(d, "noms", "manifest");
        assert_ne!(check(&s, d), s1);
    }

    #[test]
    fn an_event_or_the_backstop_rereads_what_the_stamps_call_unchanged() {
        let t = tracker("event");
        let (d, s) = (&t.0, Signer::default());
        let m = d.join("noms/manifest");
        settle();
        let s1 = check(&s, d);
        let rewrite = |text: &str| {
            std::fs::write(&m, text).unwrap();
            settle();
            s.forge(d, "noms", "manifest");
        };
        // Every stamp field identical: only being told (or the backstop) can show it.
        rewrite("root-b");
        assert_eq!(s.signature(d).unwrap(), s1);
        s.mark(d, &Touched::Paths(vec![m.clone()]));
        let s2 = check(&s, d);
        assert_ne!(s2, s1);
        rewrite("root-c");
        s.mark(d, &Touched::Unknown);
        let s3 = check(&s, d);
        assert_ne!(s3, s2);
        rewrite("root-d");
        s.mark(d, &Touched::Paths(vec!["/elsewhere".into()]));
        let s4 = check(&s, d);
        assert_ne!(s4, s3, "a path outside the directory says nothing");
        rewrite("root-e");
        s.tree(d).lock().unwrap().last_full = Instant::now().checked_sub(FULL_EVERY);
        assert_ne!(check(&s, d), s4);
    }

    #[test]
    fn unchanged_files_are_not_read_again() {
        let t = tracker("cost");
        let (d, s) = (&t.0, Signer::default());
        s.signature(d).unwrap();
        assert!(reads(&s, d) > 20);
        settle();
        s.signature(d).unwrap();
        s.signature(d).unwrap();
        assert_eq!(reads(&s, d), 0);
        // A read's mtime bump costs one small file.
        std::fs::write(d.join("noms/manifest"), "root-a").unwrap();
        s.signature(d).unwrap();
        assert_eq!(reads(&s, d), 1);
    }

    impl Signer {
        /// Give a remembered file the stamp it has now, keeping the remembered content.
        fn forge(&self, d: &Path, dir: &str, file: &str) {
            let tree = self.tree(d);
            let mut t = tree.lock().unwrap();
            let f = t
                .top
                .dirs
                .get_mut(std::ffi::OsStr::new(dir))
                .unwrap()
                .files
                .get_mut(std::ffi::OsStr::new(file))
                .unwrap();
            let m = std::fs::symlink_metadata(d.join(dir).join(file)).unwrap();
            f.stamp = Stamp::of(&m);
        }
    }
}
