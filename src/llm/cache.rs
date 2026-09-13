//! On-disk cache for LLM verdicts.
//!
//! Two monitor runs an hour apart almost always produce identical prompts
//! for a form that didn't change. Re-sending them would be wasteful and
//! costly, so a verdict is cached under a key derived from the provider,
//! model, check name, and (already redacted) prompt text. The hash is a
//! small in-crate FNV-1a — no crypto needed, and no extra dependency.

use std::path::PathBuf;

use crate::llm::prompt::Verdict;

/// A stable hex key for the given inputs. Order-sensitive.
pub fn cache_key(provider: &str, model: &str, check: &str, text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for field in [provider, model, check, text] {
        for byte in field.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // Field separator, so ("ab","c") and ("a","bc") differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// A filesystem-backed verdict cache. A `None` directory disables caching
/// entirely (every `get` misses, every `put` is a no-op).
pub struct Cache {
    dir: Option<PathBuf>,
}

impl Cache {
    /// Wraps `dir`; `None` disables the cache.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    /// The default cache location (`$XDG_CACHE_HOME/formwatch/llm`), if a
    /// cache directory can be determined.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("formwatch").join("llm"))
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{key}.json")))
    }

    /// Looks up a cached verdict.
    pub fn get(&self, key: &str) -> Option<Verdict> {
        let path = self.path(key)?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Stores a verdict. Best-effort: a failed write is ignored.
    pub fn put(&self, key: &str, verdict: &Verdict) {
        let Some(path) = self.path(key) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string(verdict) {
            let _ = std::fs::write(path, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_deterministic_and_field_sensitive() {
        assert_eq!(
            cache_key("openai", "m", "check", "text"),
            cache_key("openai", "m", "check", "text")
        );
        assert_ne!(
            cache_key("openai", "m", "check", "text"),
            cache_key("openai", "m", "check", "other")
        );
        // Field-boundary sensitivity: concatenation must not collide.
        assert_ne!(
            cache_key("openai", "m", "ab", "c"),
            cache_key("openai", "m", "a", "bc")
        );
    }

    #[test]
    fn round_trips_through_the_filesystem() {
        let dir = std::env::temp_dir().join("formwatch-test-llm-cache");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::new(Some(dir.clone()));
        let verdict = Verdict {
            score: 3,
            issues: vec!["be specific".into()],
            summary: "ok".into(),
        };

        assert!(cache.get("missing").is_none());
        cache.put("k", &verdict);
        assert_eq!(cache.get("k"), Some(verdict));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disabled_cache_never_stores() {
        let cache = Cache::new(None);
        cache.put(
            "k",
            &Verdict {
                score: 5,
                issues: vec![],
                summary: String::new(),
            },
        );
        assert!(cache.get("k").is_none());
    }
}
