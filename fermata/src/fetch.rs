//! One keyed cache for every external lookup (ticket info, PRs): single-flight per key, the last
//! good value served stale on error, failures cached by class with backoff, all of it on disk.
//! See "Failures, staleness and logging" in DESIGN.md.
use crate::cache::DiskCache;
use crate::log::{self, Throttle};
use crate::model::Stale;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How a failure is cached: an answer from the source, or something worth retrying sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailClass {
    NotFound,
    Transient,
}

pub trait Failure: Clone + Display + Serialize + DeserializeOwned {
    fn class(&self) -> FailClass;
    /// What makes two failures "the same" in the log; the key is masked by the cache.
    fn signature(&self) -> String {
        self.to_string()
    }
    /// An expected outcome that is cached like any failure but never logged.
    fn quiet(&self) -> bool {
        false
    }
}

/// TTLs for one source: a success, and the first of each class of failure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub ok: Duration,
    pub not_found: Duration,
    pub transient: Duration,
}

impl Policy {
    /// Not-found for half the success TTL; transient for a tenth of it, at least 15 s.
    pub fn from_ttl(ok: Duration) -> Self {
        Self {
            ok,
            not_found: ok / 2,
            transient: (ok / 10).max(Duration::from_secs(15)).min(ok),
        }
    }

    /// The `streak`-th consecutive failure's TTL: doubling each time, never above a success's.
    pub fn fail_ttl(&self, class: FailClass, streak: u32) -> Duration {
        let base = match class {
            FailClass::NotFound => self.not_found,
            FailClass::Transient => self.transient,
        };
        base.saturating_mul(1 << streak.saturating_sub(1).min(16))
            .min(self.ok)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Good<T> {
    value: T,
    /// Unix milliseconds.
    at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Fail<E> {
    error: E,
    at: i64,
    streak: u32,
}

/// The last success and, when the latest outcome was a failure, that failure.
#[derive(Clone, Serialize, Deserialize)]
struct Entry<T, E> {
    good: Option<Good<T>>,
    fail: Option<Fail<E>>,
}

impl<T, E> Default for Entry<T, E> {
    fn default() -> Self {
        Self {
            good: None,
            fail: None,
        }
    }
}

/// What a lookup has to show: a value (possibly stale), an error, or both.
#[derive(Debug, Clone, PartialEq)]
pub struct Lookup<T, E> {
    pub value: Option<T>,
    pub error: Option<E>,
    /// Set when `value` is the last good one and the latest refresh failed.
    pub stale: Option<Stale>,
}

impl<T, E> Lookup<T, E> {
    /// Nothing to look up.
    pub fn none() -> Self {
        Self {
            value: None,
            error: None,
            stale: None,
        }
    }
}

/// One lookup: `scope` namespaces the key (a workspace, say), `label` names it in the log.
pub struct Req<'a> {
    pub scope: &'a str,
    pub label: &'a str,
    pub key: &'a str,
    pub policy: Policy,
    pub force: bool,
}

struct State<T, E> {
    entry: Entry<T, E>,
    loaded: bool,
    inflight: bool,
}

struct Slot<T, E> {
    st: Mutex<State<T, E>>,
    cv: Condvar,
}

/// Clears `inflight` and wakes the waiters even if the fetch panicked.
struct Inflight<'a, T, E>(&'a Slot<T, E>);

impl<T, E> Drop for Inflight<'_, T, E> {
    fn drop(&mut self) {
        if let Ok(mut st) = self.0.st.lock() {
            st.inflight = false;
        }
        self.0.cv.notify_all();
    }
}

pub struct KeyedCache<T, E> {
    /// Names the source in log lines ("ticket info").
    source: &'static str,
    /// Disk entry prefix; bump it when the stored shape changes.
    prefix: &'static str,
    slots: Mutex<HashMap<String, Arc<Slot<T, E>>>>,
    pub disk: Option<DiskCache>,
    /// Fetches started and log lines written, for tests.
    pub fetches: AtomicUsize,
    pub logged: AtomicUsize,
    failures: Throttle,
    /// stderr on a success: worth a line, not one per key per TTL.
    notes: Throttle,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339()
}

impl<T, E> KeyedCache<T, E>
where
    T: Clone + Serialize + DeserializeOwned,
    E: Failure,
{
    pub fn new(source: &'static str, prefix: &'static str) -> Self {
        Self {
            source,
            prefix,
            slots: Mutex::new(HashMap::new()),
            disk: None,
            fetches: AtomicUsize::new(0),
            logged: AtomicUsize::new(0),
            failures: Throttle::default(),
            notes: Throttle::default(),
        }
    }

    fn emit(&self, line: Option<String>) {
        if let Some(l) = line {
            self.logged.fetch_add(1, Ordering::Relaxed);
            log::warn(l);
        }
    }

    fn slot(&self, full: &str) -> Arc<Slot<T, E>> {
        self.slots
            .lock()
            .unwrap()
            .entry(full.to_string())
            .or_insert_with(|| {
                Arc::new(Slot {
                    st: Mutex::new(State {
                        entry: Entry::default(),
                        loaded: false,
                        inflight: false,
                    }),
                    cv: Condvar::new(),
                })
            })
            .clone()
    }

    fn disk_name(&self, full: &str) -> String {
        format!("{}-{full}", self.prefix)
    }

    fn fresh(e: &Entry<T, E>, p: &Policy, now: i64) -> bool {
        let within = |at: i64, ttl: Duration| now - at < ttl.as_millis() as i64;
        match (&e.fail, &e.good) {
            (Some(f), _) => within(f.at, p.fail_ttl(f.error.class(), f.streak)),
            (None, Some(g)) => within(g.at, p.ok),
            (None, None) => false,
        }
    }

    fn lookup(e: &Entry<T, E>) -> Lookup<T, E> {
        let stale = match (&e.good, &e.fail) {
            (Some(g), Some(f)) => Some(Stale {
                as_of: rfc3339(g.at),
                error: f.error.to_string(),
            }),
            _ => None,
        };
        Lookup {
            value: e.good.as_ref().map(|g| g.value.clone()),
            error: e.fail.as_ref().map(|f| f.error.clone()),
            stale,
        }
    }

    /// The cached answer for `r`, fetching it with `fetch` when it is due (or forced) and no
    /// fetch for the key is running; otherwise waits for the running one. `fetch` returns the
    /// result plus detail for the log line (exit status, stderr), empty if none.
    pub fn get(&self, r: &Req, fetch: impl FnOnce() -> (Result<T, E>, String)) -> Lookup<T, E> {
        let full = format!("{}|{}", r.scope, r.key);
        let slot = self.slot(&full);
        let mut st = slot.st.lock().unwrap();
        if !st.loaded {
            st.loaded = true;
            if let Some(d) = &self.disk
                && let Some(e) = d.get::<Entry<T, E>>(&self.disk_name(&full), &full, None)
            {
                st.entry = e;
            }
        }
        if st.inflight {
            let st = slot.cv.wait_while(st, |s| s.inflight).unwrap();
            return Self::lookup(&st.entry);
        }
        if !r.force && Self::fresh(&st.entry, &r.policy, now_ms()) {
            return Self::lookup(&st.entry);
        }
        st.inflight = true;
        drop(st);
        let _guard = Inflight(&slot);
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let (res, detail) = fetch();
        let now = now_ms();
        let mut st = slot.st.lock().unwrap();
        let label = if r.label.is_empty() {
            String::new()
        } else {
            format!(" [{}]", r.label)
        };
        let src = format!("{}{label}", self.source);
        match res {
            Ok(v) => {
                st.entry.good = Some(Good { value: v, at: now });
                st.entry.fail = None;
                self.emit(self.failures.ok(&full));
                if !detail.is_empty() {
                    let masked = detail.replace(r.key, "{key}");
                    let sig = format!("{}|{}|{masked}", self.source, r.scope);
                    let what = format!("{src}{masked}");
                    let first = format!("{src} {}{detail}", r.key);
                    self.emit(self.notes.hit(&sig, &what, &full, first));
                }
            }
            Err(e) => {
                let streak = st.entry.fail.as_ref().map_or(1, |f| f.streak + 1);
                let sig = format!(
                    "{}|{}|{}",
                    self.source,
                    r.scope,
                    e.signature().replace(r.key, "{key}")
                );
                let what = format!("{src}: {}", e.to_string().replace(r.key, "{key}"));
                let first = format!("{src} {}: {e}{detail}", r.key);
                if !e.quiet() {
                    self.emit(self.failures.hit(&sig, &what, &full, first));
                }
                st.entry.fail = Some(Fail {
                    error: e,
                    at: now,
                    streak,
                });
            }
        }
        if let Some(d) = &self.disk {
            d.put(&self.disk_name(&full), &full, &st.entry);
        }
        Self::lookup(&st.entry)
    }

    /// Pretend the entry's outcomes happened `ms` earlier.
    #[cfg(test)]
    pub fn age(&self, scope: &str, key: &str, ms: i64) {
        let slot = self.slot(&format!("{scope}|{key}"));
        let mut st = slot.st.lock().unwrap();
        if let Some(g) = st.entry.good.as_mut() {
            g.at -= ms;
        }
        if let Some(f) = st.entry.fail.as_mut() {
            f.at -= ms;
        }
    }

    /// Seed a good value, as if fetched now.
    #[cfg(test)]
    pub fn seed(&self, scope: &str, key: &str, v: T) {
        let slot = self.slot(&format!("{scope}|{key}"));
        let mut st = slot.st.lock().unwrap();
        st.loaded = true;
        st.entry = Entry {
            good: Some(Good {
                value: v,
                at: now_ms(),
            }),
            fail: None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// "gone" is a not-found; anything else is transient.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct E(String);

    impl Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl Failure for E {
        fn class(&self) -> FailClass {
            if self.0 == "gone" {
                FailClass::NotFound
            } else {
                FailClass::Transient
            }
        }
    }

    const MIN: i64 = 60_000;

    fn req(force: bool) -> Req<'static> {
        Req {
            scope: "ws",
            label: "ctx",
            key: "K-1",
            policy: Policy::from_ttl(Duration::from_secs(300)),
            force,
        }
    }

    type C = KeyedCache<String, E>;

    fn ok(v: &str) -> impl FnOnce() -> (Result<String, E>, String) {
        let v = v.to_string();
        move || (Ok(v), String::new())
    }

    fn fail(e: &str) -> impl FnOnce() -> (Result<String, E>, String) {
        let e = E(e.to_string());
        move || (Err(e), " (exit 1)".into())
    }

    #[test]
    fn policy_ttls_and_backoff() {
        let p = Policy::from_ttl(Duration::from_secs(300));
        let s = |c, n| p.fail_ttl(c, n).as_secs();
        assert_eq!(
            (1..=6)
                .map(|n| s(FailClass::Transient, n))
                .collect::<Vec<_>>(),
            vec![30, 60, 120, 240, 300, 300]
        );
        assert_eq!(
            (s(FailClass::NotFound, 1), s(FailClass::NotFound, 2)),
            (150, 300)
        );
        let p = Policy::from_ttl(Duration::from_secs(60));
        assert_eq!((p.not_found.as_secs(), p.transient.as_secs()), (30, 15));
        assert_eq!(p.fail_ttl(FailClass::Transient, 1000), p.ok);
    }

    #[test]
    fn stale_on_error_then_recovery() {
        let c = C::new("t", "x");
        let r = c.get(&req(false), ok("v1"));
        assert_eq!((r.value.as_deref(), r.stale), (Some("v1"), None));
        // Fresh: no fetch.
        c.get(&req(false), || panic!("fresh"));
        // Expired, and the source is down: the last good value, marked stale.
        c.age("ws", "K-1", 6 * MIN);
        let r = c.get(&req(false), fail("down"));
        assert_eq!(r.value.as_deref(), Some("v1"));
        assert_eq!(r.error, Some(E("down".into())));
        assert_eq!(r.stale.as_ref().unwrap().error, "down");
        // The failure is cached (30 s), so the next refresh asks nothing.
        c.get(&req(false), || panic!("negative-cached"));
        // After it, a success clears the staleness.
        c.age("ws", "K-1", MIN);
        let r = c.get(&req(false), ok("v2"));
        assert_eq!(
            (r.value.as_deref(), r.error, r.stale),
            (Some("v2"), None, None)
        );
        assert_eq!(c.fetches.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn never_succeeded_shows_only_the_error() {
        let c = C::new("t", "x");
        let r = c.get(&req(false), fail("down"));
        assert_eq!(
            (r.value, r.error, r.stale),
            (None, Some(E("down".into())), None)
        );
        assert_eq!(
            c.get(&req(false), || panic!()).error,
            Some(E("down".into()))
        );
    }

    #[test]
    fn repeated_failures_back_off() {
        let c = C::new("t", "x");
        c.get(&req(false), fail("down"));
        // 1st failure: 30 s. Just past it, a 2nd failure: 60 s.
        c.age("ws", "K-1", 31_000);
        c.get(&req(false), fail("down"));
        c.age("ws", "K-1", 31_000);
        c.get(&req(false), || panic!("backed off to 60 s"));
        c.age("ws", "K-1", 30_000);
        c.get(&req(false), fail("down"));
        assert_eq!(c.fetches.load(Ordering::Relaxed), 3);
        // Not-found is an answer: cached 150 s from its first occurrence.
        let c = C::new("t", "x");
        c.get(&req(false), fail("gone"));
        c.age("ws", "K-1", 2 * MIN);
        c.get(&req(false), || panic!("not-found cached 150 s"));
        // Force always asks.
        c.get(&req(true), ok("v"));
        assert_eq!(c.fetches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn failures_persist_across_restart() {
        let d = std::env::temp_dir().join(format!("fermata-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mk = || {
            let mut c = C::new("t", "x");
            c.disk = Some(DiskCache::new(&d));
            c
        };
        let c = mk();
        c.get(&req(false), ok("v1"));
        c.age("ws", "K-1", 6 * MIN);
        c.get(&req(false), fail("down"));
        // A restart: the failure is still cached, and the last good value still served stale.
        let c = mk();
        let r = c.get(&req(false), || panic!("negative result kept on disk"));
        assert_eq!(r.value.as_deref(), Some("v1"));
        assert!(r.stale.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Concurrent requests for one key share the fetch already running.
    #[test]
    fn single_flight() {
        let c = C::new("t", "x");
        let started = AtomicBool::new(false);
        let rs: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|i| {
                    let (c, started) = (&c, &started);
                    sc.spawn(move || {
                        if i > 0 {
                            while !started.load(Ordering::SeqCst) {
                                std::thread::yield_now();
                            }
                        }
                        c.get(&req(i % 2 == 0), || {
                            started.store(true, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(200));
                            (Ok::<_, E>("v".to_string()), String::new())
                        })
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(c.fetches.load(Ordering::Relaxed), 1);
        assert!(rs.iter().all(|r| r.value.as_deref() == Some("v")));
    }

    #[test]
    fn keys_are_scoped() {
        let c = C::new("t", "x");
        c.get(&req(false), ok("a"));
        let r = c.get(
            &Req {
                scope: "other-ws",
                ..req(false)
            },
            ok("b"),
        );
        assert_eq!(r.value.as_deref(), Some("b"));
        assert_eq!(c.fetches.load(Ordering::Relaxed), 2);
    }
}
