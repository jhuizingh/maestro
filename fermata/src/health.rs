//! Per-source health: the last success, the last failure and its kind, the failing keys and the
//! age of the data served. Every external call reports here; `H` and `--doctor` read it.
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// `fermata <version> (<commit>, built <time>)`.
pub fn build_line() -> String {
    format!(
        "fermata {} ({}, built {})",
        env!("CARGO_PKG_VERSION"),
        env!("FERMATA_COMMIT"),
        env!("FERMATA_BUILT")
    )
}

/// A keyed source that is not asked again for this long has dropped out of view.
const FORGET: Duration = Duration::hours(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Tracker,
    Tickets,
    Review,
    Gh,
    Sessions,
    Tmux,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Tracker => "tracker",
            Source::Tickets => "ticket adapter",
            Source::Review => "review rank",
            Source::Gh => "gh",
            Source::Sessions => "session records",
            Source::Tmux => "tmux",
        }
    }

    /// Re-asked on a TTL while its task is visible, so a silent failing key can be dropped.
    fn forgets(self) -> bool {
        matches!(self, Source::Tickets | Source::Review | Source::Gh)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: String,
    pub key: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceHealth {
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<Failure>,
    /// Keys whose latest call failed, with when it failed.
    pub failing: BTreeMap<String, DateTime<Utc>>,
    /// When the data now served was fetched (a success, or a disk cache entry's save time).
    pub data_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// A context name; empty for a global source.
    pub scope: String,
    pub source: Source,
    pub degraded: bool,
    pub data_age_s: Option<i64>,
    #[serde(flatten)]
    pub health: SourceHealth,
}

impl Row {
    pub fn state(&self) -> String {
        match (&self.health.last_ok, &self.health.last_error) {
            _ if self.degraded => {
                let n = self.health.failing.len();
                if n == 1 {
                    "degraded".into()
                } else {
                    format!("degraded ({n} failing)")
                }
            }
            (None, None) if self.health.data_at.is_some() => "cached".into(),
            (None, None) => "not asked yet".into(),
            _ => "ok".into(),
        }
    }
}

/// Classify a non-ticket failure message.
pub fn kind_of(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if m.contains("could not resolve to a") || m.contains("no pull requests found") {
        "not_found"
    } else if m.contains("timed out") || m.contains("timeout") {
        "timeout"
    } else if m.contains("auth") || m.contains("401") || m.contains("credentials") {
        "auth"
    } else if [
        "could not resolve",
        "error connecting",
        "connection",
        "network",
        "unreachable",
    ]
    .iter()
    .any(|s| m.contains(s))
    {
        "network"
    } else {
        "error"
    }
}

#[derive(Default)]
pub struct Health {
    map: Mutex<BTreeMap<(String, Source), SourceHealth>>,
}

impl Health {
    /// Make the row exist, so a source that was never called reads "not asked yet".
    pub fn expect(&self, scope: &str, src: Source) {
        self.map
            .lock()
            .unwrap()
            .entry((scope.to_string(), src))
            .or_default();
    }

    pub fn ok(&self, scope: &str, src: Source, key: &str) {
        self.ok_at(scope, src, key, Utc::now());
    }

    fn ok_at(&self, scope: &str, src: Source, key: &str, now: DateTime<Utc>) {
        let mut m = self.map.lock().unwrap();
        let e = m.entry((scope.to_string(), src)).or_default();
        e.last_ok = Some(now);
        e.data_at = Some(now);
        e.failing.remove(key);
    }

    pub fn fail(&self, scope: &str, src: Source, key: &str, kind: &str, message: &str) {
        self.fail_at(scope, src, key, kind, message, Utc::now());
    }

    fn fail_at(
        &self,
        scope: &str,
        src: Source,
        key: &str,
        kind: &str,
        message: &str,
        now: DateTime<Utc>,
    ) {
        let message = message
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        let mut m = self.map.lock().unwrap();
        let e = m.entry((scope.to_string(), src)).or_default();
        e.last_error = Some(Failure {
            at: now,
            kind: kind.to_string(),
            message,
            key: key.to_string(),
        });
        e.failing.insert(key.to_string(), now);
    }

    /// Record a result, classifying an error by its message.
    pub fn record<T, E: std::fmt::Display>(
        &self,
        scope: &str,
        src: Source,
        key: &str,
        r: &Result<T, E>,
    ) {
        match r {
            Ok(_) => self.ok(scope, src, key),
            Err(e) => {
                let m = e.to_string();
                self.fail(scope, src, key, kind_of(&m), &m);
            }
        }
    }

    /// The data served came from a cache saved at `at`; nothing was asked.
    pub fn cached(&self, scope: &str, src: Source, at: DateTime<Utc>) {
        let mut m = self.map.lock().unwrap();
        let e = m.entry((scope.to_string(), src)).or_default();
        if e.data_at.is_none_or(|d| d < at) {
            e.data_at = Some(at);
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        self.rows_at(Utc::now())
    }

    fn rows_at(&self, now: DateTime<Utc>) -> Vec<Row> {
        let mut m = self.map.lock().unwrap();
        m.iter_mut()
            .map(|((scope, src), h)| {
                if src.forgets() {
                    h.failing.retain(|_, at| now - *at < FORGET);
                }
                Row {
                    scope: scope.clone(),
                    source: *src,
                    degraded: !h.failing.is_empty(),
                    data_age_s: h.data_at.map(|d| (now - d).num_seconds().max(0)),
                    health: h.clone(),
                }
            })
            .collect()
    }
}

/// Where fermata reads and writes, for the diagnostics header.
pub fn paths(baton: Option<&std::path::Path>) -> Vec<(&'static str, String)> {
    let show = |p: &std::path::Path| p.display().to_string();
    vec![
        (
            "baton",
            baton.map(show).unwrap_or_else(|| "not found".into()),
        ),
        ("sessions", show(&crate::store::data_dir())),
        ("ccmon", show(&crate::sessions::ccmon_dir())),
        ("cache", show(&crate::cache::DiskCache::default_dir())),
        ("log", show(&crate::log::path())),
    ]
}

/// One row's cells: context, source, state, last success, data age, last error.
pub fn cells(r: &crate::api::Source, now: DateTime<Utc>) -> [String; 6] {
    let age = |t: &str| crate::util::parse_ts(t).map(|t| crate::util::rel_age(t, now));
    let of = |t: &Option<String>| t.as_deref().and_then(age);
    let err = r.last_error.as_ref().map(|e| {
        let ago = age(&e.at).unwrap_or_default();
        format!("{ago} ago {} [{}]: {}", e.kind, e.key, e.message)
    });
    let none = || "-".to_string();
    let scope = if r.scope.is_empty() {
        "(all)"
    } else {
        &r.scope
    };
    [
        scope.to_string(),
        r.source.clone(),
        r.state.clone(),
        of(&r.last_ok)
            .map(|a| format!("{a} ago"))
            .unwrap_or_else(none),
        of(&r.data_at).unwrap_or_else(none),
        err.unwrap_or_else(none),
    ]
}

pub const HEADERS: [&str; 6] = [
    "CONTEXT",
    "SOURCE",
    "STATE",
    "LAST OK",
    "DATA AGE",
    "LAST ERROR",
];

/// The `--doctor` report: build, paths, then one aligned line per source.
pub fn report(rows: &[Row], paths: &[(&str, String)]) -> String {
    let now = Utc::now();
    let mut out = format!("{}\n", build_line());
    for (k, v) in paths {
        out += &format!("{k:<6} {v}\n");
    }
    out += "\n";
    let mut table: Vec<[String; 6]> = vec![HEADERS.map(String::from)];
    table.extend(rows.iter().map(|r| cells(&crate::project::source(r), now)));
    let w: Vec<usize> = (0..5)
        .map(|i| {
            table
                .iter()
                .map(|c| c[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for c in &table {
        let mut line = String::new();
        for i in 0..5 {
            line += &format!("{:<1$}  ", c[i], w[i]);
        }
        out += &format!("{line}{}\n", c[5]);
    }
    let n = rows.iter().filter(|r| r.degraded).count();
    out += &match n {
        0 => "\nall sources ok\n".to_string(),
        1 => "\n1 source degraded\n".to_string(),
        n => format!("\n{n} sources degraded\n"),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_is_degraded_while_any_key_fails() {
        let h = Health::default();
        h.expect("work", Source::Tickets);
        assert_eq!(h.rows()[0].state(), "not asked yet");
        h.ok("work", Source::Tickets, "A-1");
        h.fail(
            "work",
            Source::Tickets,
            "A-2",
            "unavailable",
            "vpn down\nmore",
        );
        let r = &h.rows()[0];
        assert!(r.degraded);
        let e = r.health.last_error.as_ref().unwrap();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("unavailable", "vpn down")
        );
        h.ok("work", Source::Tickets, "A-1");
        assert!(
            h.rows()[0].degraded,
            "another key's success does not clear A-2"
        );
        h.ok("work", Source::Tickets, "A-2");
        assert!(!h.rows()[0].degraded);
        assert_eq!(h.rows()[0].state(), "ok");
        assert!(
            h.rows()[0].health.last_error.is_some(),
            "the last error is kept"
        );
    }

    #[test]
    fn silent_keyed_failures_are_forgotten_but_tracker_ones_are_not() {
        let h = Health::default();
        let then = Utc::now() - Duration::hours(2);
        h.fail_at("w", Source::Gh, "o/r#1", "error", "x", then);
        h.fail_at("w", Source::Tracker, "list", "error", "x", then);
        let rows = h.rows();
        let get = |s| rows.iter().find(|r| r.source == s).unwrap();
        assert!(!get(Source::Gh).degraded);
        assert!(get(Source::Tracker).degraded);
    }

    #[test]
    fn data_age_comes_from_the_newest_fetch_or_cache() {
        let h = Health::default();
        let now = Utc::now();
        h.cached("w", Source::Tracker, now - Duration::seconds(300));
        assert_eq!(h.rows_at(now)[0].data_age_s, Some(300));
        assert_eq!(h.rows_at(now)[0].state(), "cached");
        h.cached("w", Source::Tracker, now - Duration::seconds(900));
        assert_eq!(
            h.rows_at(now)[0].data_age_s,
            Some(300),
            "an older cache never wins"
        );
        h.ok_at("w", Source::Tracker, "list", now - Duration::seconds(10));
        assert_eq!(h.rows_at(now)[0].data_age_s, Some(10));
    }

    #[test]
    fn report_lists_each_source_and_the_verdict() {
        let h = Health::default();
        h.ok("work", Source::Tracker, "list");
        h.fail("", Source::Gh, "o/r#3", "auth", "gh auth login");
        let out = report(&h.rows(), &[("log", "/x/fermata.log".into())]);
        assert!(out.starts_with("fermata "));
        assert!(out.contains("log    /x/fermata.log"));
        let gh = out.lines().find(|l| l.contains(" gh ")).unwrap();
        assert!(gh.starts_with("(all)") && gh.contains("degraded"));
        assert!(gh.contains("auth [o/r#3]: gh auth login"));
        assert!(
            out.lines()
                .any(|l| l.starts_with("work") && l.contains("tracker"))
        );
        assert!(out.ends_with("1 source degraded\n"));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("gh timed out after 15s"), "timeout");
        assert_eq!(
            kind_of("To get started with GitHub CLI, please run: gh auth login"),
            "auth"
        );
        assert_eq!(kind_of("error connecting to api.github.com"), "network");
        assert_eq!(
            kind_of("dial tcp: lookup x: could not resolve host"),
            "network"
        );
        assert_eq!(kind_of("exit 1"), "error");
        let gone = "GraphQL: Could not resolve to a PullRequest with the number of 3.";
        assert_eq!(kind_of(gone), "not_found");
        assert_eq!(
            kind_of("no pull requests found for branch \"b\""),
            "not_found"
        );
    }

    #[test]
    fn build_line_names_version_commit_and_time() {
        let l = build_line();
        assert!(l.starts_with(&format!("fermata {} (", env!("CARGO_PKG_VERSION"))));
        assert!(l.contains(", built ") && l.ends_with(" UTC)"));
    }
}
