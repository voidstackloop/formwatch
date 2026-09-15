//! Deterministic work partitioning for large registries.
//!
//! A registry of a few thousand forms doesn't have to run on one machine.
//! `monitor --shard 2/5` selects a stable subset of the configured forms
//! by position, so the same registry can be split across several CI jobs
//! (or runners) with no coordination: the union of all shards is the whole
//! set, and each form belongs to exactly one shard on every run.
//!
//! Partitioning is by position in the resolved form list, not by URL hash,
//! so it's trivial to reason about ("shard 1 gets items 0, 5, 10, ...") and
//! independent of the URL's shape.

use std::str::FromStr;

/// A 1-based slice of a list: `index` of `total` shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Shard {
    /// 1-based shard number.
    index: usize,
    /// Total number of shards.
    total: usize,
}

impl Shard {
    /// Validates and builds a shard. `index` must be in `1..=total` and
    /// `total` must be at least 1.
    pub fn new(index: usize, total: usize) -> Result<Self, String> {
        if total == 0 {
            return Err("shard total must be at least 1".to_string());
        }
        if index == 0 || index > total {
            return Err(format!(
                "shard index {index} is out of range for {total} shard(s)"
            ));
        }
        Ok(Self { index, total })
    }

    /// Selects this shard's slice of `items`, preserving order.
    pub fn select<T>(&self, items: Vec<T>) -> Vec<T> {
        if self.total <= 1 {
            return items;
        }
        items
            .into_iter()
            .enumerate()
            // `index` is 1-based and validated by `new`; `saturating_sub`
            // is pure defense-in-depth against a hand-built value
            // underflowing (and in release, wrapping so every form is
            // silently skipped).
            .filter(|(i, _)| i % self.total == self.index.saturating_sub(1))
            .map(|(_, item)| item)
            .collect()
    }
}

impl std::fmt::Display for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.index, self.total)
    }
}

impl FromStr for Shard {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (index, total) = s
            .trim()
            .split_once('/')
            .ok_or_else(|| format!("expected INDEX/TOTAL, got {s:?}"))?;
        let index: usize = index
            .trim()
            .parse()
            .map_err(|_| format!("invalid shard index in {s:?}"))?;
        let total: usize = total
            .trim()
            .parse()
            .map_err(|_| format!("invalid shard total in {s:?}"))?;
        Shard::new(index, total)
    }
}

impl TryFrom<String> for Shard {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Shard> for String {
    fn from(shard: Shard) -> Self {
        shard.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_index_slash_total_form() {
        assert_eq!("2/5".parse::<Shard>(), Ok(Shard { index: 2, total: 5 }));
        assert_eq!(" 1 / 1 ".parse::<Shard>(), Ok(Shard { index: 1, total: 1 }));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        assert!("0/5".parse::<Shard>().is_err());
        assert!("6/5".parse::<Shard>().is_err());
        assert!("1/0".parse::<Shard>().is_err());
        assert!("nope".parse::<Shard>().is_err());
        assert!("a/b".parse::<Shard>().is_err());
    }

    #[test]
    fn selecting_all_shards_partitions_every_item_exactly_once() {
        let total = 3;
        let items: Vec<usize> = (0..20).collect();
        let mut union = Vec::new();
        for index in 1..=total {
            let shard = Shard::new(index, total).unwrap();
            union.extend(shard.select(items.clone()));
        }
        union.sort_unstable();
        assert_eq!(union, items, "union of all shards must be the whole set");
    }

    #[test]
    fn one_shard_returns_everything_and_empty_is_empty() {
        let shard = Shard::new(1, 1).unwrap();
        assert_eq!(shard.select(vec![1, 2, 3]), vec![1, 2, 3]);
        let shard = Shard::new(2, 3).unwrap();
        assert!(shard.select(Vec::<i32>::new()).is_empty());
    }

    #[test]
    fn serializes_as_a_string() {
        let shard = Shard::new(2, 5).unwrap();
        assert_eq!(serde_json::to_string(&shard).unwrap(), "\"2/5\"");
        let back: Shard = serde_json::from_str("\"2/5\"").unwrap();
        assert_eq!(back, shard);
    }
}
