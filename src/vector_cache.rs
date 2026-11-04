//! Session-local optimization. Keys are hashes, never input text. No disk I/O.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(crate) struct CachedVector {
    pub vector: [f32; 384],
    pub original: usize,
    pub used: usize,
    pub truncated: bool,
}

struct Entry {
    value: CachedVector,
    expires: Instant,
    touched: u64,
}

// Includes fixed vector/key/metadata plus a conservative allowance per tree node.
// This is accounted retained storage, not an allocator/RSS limit.
const ENTRY_BYTES: usize = std::mem::size_of::<Entry>() + 32 + 256;

#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheStats {
    pub enabled: bool,
    pub entries: usize,
    pub accounted_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evicted: u64,
    pub expired: u64,
    pub skipped_oversize: u64,
}

pub(crate) struct VectorCache {
    entries: BTreeMap<[u8; 32], Entry>,
    max_bytes: usize,
    ttl: Duration,
    tick: u64,
    next_expiry: Option<Instant>,
    stats: CacheStats,
}

impl VectorCache {
    pub fn new(max_bytes: usize, ttl: Duration) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_bytes,
            ttl,
            tick: 0,
            next_expiry: None,
            stats: CacheStats {
                enabled: max_bytes > 0 && !ttl.is_zero(),
                ..Default::default()
            },
        }
    }
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            accounted_bytes: self.entries.len() * ENTRY_BYTES,
            ..self.stats.clone()
        }
    }
    pub fn purge(&mut self, now: Instant) {
        if self.next_expiry.is_some_and(|expiry| now >= expiry) {
            let before = self.entries.len();
            self.entries.retain(|_, e| now < e.expires);
            self.stats.expired += (before - self.entries.len()) as u64;
            self.next_expiry = self.entries.values().map(|e| e.expires).min();
        }
    }
    pub fn get(&mut self, key: &[u8; 32], now: Instant) -> Option<CachedVector> {
        self.purge(now);
        if !self.stats.enabled {
            return None;
        }
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.touched = self.tick;
            self.stats.hits += 1;
            Some(entry.value.clone())
        } else {
            self.stats.misses += 1;
            None
        }
    }
    pub fn insert(&mut self, key: [u8; 32], value: CachedVector, now: Instant) {
        self.purge(now);
        if !self.stats.enabled {
            return;
        }
        if ENTRY_BYTES > self.max_bytes {
            self.stats.skipped_oversize += 1;
            return;
        }
        if !self.entries.contains_key(&key) && self.entries.len() >= self.max_bytes / ENTRY_BYTES {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| *k)
            {
                self.entries.remove(&oldest);
                self.stats.evicted += 1;
            }
        }
        self.tick += 1;
        let expires = now + self.ttl;
        self.entries.insert(
            key,
            Entry {
                value,
                expires,
                touched: self.tick,
            },
        );
        self.next_expiry = Some(self.next_expiry.map_or(expires, |old| old.min(expires)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn value() -> CachedVector {
        CachedVector {
            vector: [0.25; 384],
            original: 10,
            used: 10,
            truncated: false,
        }
    }
    #[test]
    fn absolute_expiry_lru_copy_and_budget() {
        let now = Instant::now();
        let mut cache = VectorCache::new(ENTRY_BYTES * 2, Duration::from_secs(10));
        cache.insert([1; 32], value(), now);
        cache.insert([2; 32], value(), now);
        let mut copy = cache.get(&[1; 32], now + Duration::from_secs(9)).unwrap();
        copy.vector[0] = 999.;
        assert_eq!(copy.vector[0], 999.);
        cache.insert([3; 32], value(), now + Duration::from_secs(9));
        assert!(cache.get(&[2; 32], now + Duration::from_secs(9)).is_none());
        assert_eq!(
            cache
                .get(&[1; 32], now + Duration::from_secs(9))
                .unwrap()
                .vector[0],
            0.25
        );
        // Reading does not extend absolute TTL. Expiry is inclusive at the boundary.
        assert!(cache.get(&[1; 32], now + Duration::from_secs(10)).is_none());
        assert_eq!(cache.stats().expired, 1);
        assert_eq!(cache.stats().evicted, 1);
        assert_eq!(cache.stats().entries, 1);
        assert!(cache.stats().accounted_bytes <= ENTRY_BYTES * 2);
        cache.purge(now + Duration::from_secs(19));
        assert_eq!(cache.stats().accounted_bytes, 0);
    }
    #[test]
    fn disabled_and_oversized_never_retain() {
        for (budget, ttl) in [(0, 10), (ENTRY_BYTES, 0), (ENTRY_BYTES - 1, 10)] {
            let mut cache = VectorCache::new(budget, Duration::from_secs(ttl));
            cache.insert([0; 32], value(), Instant::now());
            assert!(cache.get(&[0; 32], Instant::now()).is_none());
            assert_eq!(cache.stats().entries, 0);
        }
    }
}
