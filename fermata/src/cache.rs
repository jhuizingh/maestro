//! Small on-disk cache so a cold start (and `--dump`) reuses recent results.
//! Each entry is `<dir>/<name>.json` = `{key, saved_at, value}`.
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct DiskCache {
    pub dir: PathBuf,
}

fn safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl DiskCache {
    pub fn default_dir() -> PathBuf {
        if let Ok(p) = std::env::var("FERMATA_CACHE_DIR") {
            return PathBuf::from(p);
        }
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("fermata")
            .join("cache")
    }

    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.json", safe(name)))
    }

    /// The value, if its key matches and it is younger than `max_age_s` (None = any age).
    pub fn get<T: DeserializeOwned>(
        &self,
        name: &str,
        key: &str,
        max_age_s: Option<u64>,
    ) -> Option<T> {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(self.path(name)).ok()?).ok()?;
        if v.get("key")?.as_str()? != key {
            return None;
        }
        if let Some(max) = max_age_s {
            let saved = v.get("saved_at")?.as_i64()?;
            if chrono::Utc::now().timestamp() - saved > max as i64 {
                return None;
            }
        }
        serde_json::from_value(v.get("value")?.clone()).ok()
    }

    /// The last stored value regardless of key or age (for warming caches early).
    pub fn get_any<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(self.path(name)).ok()?).ok()?;
        serde_json::from_value(v.get("value")?.clone()).ok()
    }

    pub fn put<T: Serialize>(&self, name: &str, key: &str, value: &T) {
        let _ = std::fs::create_dir_all(&self.dir);
        let body = serde_json::json!({
            "key": key,
            "saved_at": chrono::Utc::now().timestamp(),
            "value": value,
        });
        let p = self.path(name);
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, body.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_key_and_age() {
        let d = std::env::temp_dir().join(format!("fermata-cache-{}", std::process::id()));
        let c = DiskCache::new(&d);
        c.put("a/b", "k1", &vec![1, 2]);
        assert_eq!(c.get::<Vec<i32>>("a/b", "k1", Some(60)), Some(vec![1, 2]));
        assert_eq!(c.get::<Vec<i32>>("a/b", "k2", None), None);
        assert_eq!(c.get::<Vec<i32>>("missing", "k1", None), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
