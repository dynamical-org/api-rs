//! A bounded cache of opaque dataset handles. Resolver/opener closures own all
//! storage I/O. Identical keys coalesce; unrelated keys never wait on that I/O.
use crate::{
    CatalogError, Clock, Collection, RepositoryLocation, VirtualChunkContainer,
    collection::python_repr,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex, Weak},
};

pub const DEFAULT_MAIN_TTL_SECONDS: f64 = 30.0;
pub const DEFAULT_DATASET_CACHE_ENTRIES: usize = 32;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    Default,
    Routing,
    /// Stored coordinate metadata with no virtual-container authorization.
    Metadata,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryKey {
    pub collection_id: String,
    pub location: RepositoryLocation,
    pub profile: Profile,
    pub containers: Vec<VirtualChunkContainer>,
    /// Exact freshly captured source policy; legacy/metadata recipes use None.
    pub source_policy_id: Option<[u8; 32]>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DatasetKey {
    pub collection_id: String,
    pub location: RepositoryLocation,
    pub snapshot_id: Option<String>,
    pub profile: Profile,
    pub containers: Vec<VirtualChunkContainer>,
    /// Exact freshly captured source policy; legacy/metadata recipes use None.
    pub source_policy_id: Option<[u8; 32]>,
}
impl DatasetKey {
    pub fn new(collection: &Collection, snapshot_id: Option<&str>, profile: Profile) -> Self {
        let mut containers = collection.virtual_chunk_containers();
        containers.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.url_prefix.cmp(&b.url_prefix)));
        Self {
            collection_id: collection.id.clone(),
            location: collection.icechunk.clone(),
            snapshot_id: snapshot_id.map(str::to_owned),
            profile,
            containers,
            source_policy_id: None,
        }
    }
    pub fn with_source_policy(mut self, policy_id: [u8; 32]) -> Self {
        self.source_policy_id = Some(policy_id);
        self
    }
    pub fn repository_key(&self) -> RepositoryKey {
        RepositoryKey {
            collection_id: self.collection_id.clone(),
            location: self.location.clone(),
            profile: self.profile,
            containers: self.containers.clone(),
            source_policy_id: self.source_policy_id,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheStatus {
    Hit,
    Miss,
    Revalidated,
    Unknown,
}
#[derive(Debug)]
pub struct OpenedDataset<H> {
    pub dataset: Arc<H>,
    pub snapshot_id: String,
    pub cache_status: CacheStatus,
}
impl<H> Clone for OpenedDataset<H> {
    fn clone(&self) -> Self {
        Self {
            dataset: self.dataset.clone(),
            snapshot_id: self.snapshot_id.clone(),
            cache_status: self.cache_status,
        }
    }
}
/// A resolved branch or pinned snapshot and the caller's opaque read session.
pub struct Resolved<S> {
    pub snapshot_id: String,
    pub session: S,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownSnapshotError {
    pub snapshot_id: String,
}
impl std::fmt::Display for UnknownSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unknown snapshot {}", python_repr(&self.snapshot_id))
    }
}
impl std::error::Error for UnknownSnapshotError {}
struct Cached<H> {
    opened: OpenedDataset<H>,
    expires_at: f64,
    bytes: usize,
}

#[derive(Default)]
struct KeyedLocks<K> {
    entries: Mutex<HashMap<K, Weak<Mutex<()>>>>,
}
struct KeyLock<'a, K: Eq + Hash> {
    pool: &'a KeyedLocks<K>,
    key: K,
    lock: Arc<Mutex<()>>,
}
impl<K: Clone + Eq + Hash> KeyedLocks<K> {
    fn entry(&self, key: &K) -> KeyLock<'_, K> {
        let mut entries = self.entries.lock().unwrap();
        let lock = entries.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(Mutex::new(()));
            entries.insert(key.clone(), Arc::downgrade(&lock));
            lock
        });
        KeyLock {
            pool: self,
            key: key.clone(),
            lock,
        }
    }
}
impl<K: Eq + Hash> Drop for KeyLock<'_, K> {
    fn drop(&mut self) {
        let mut entries = self.pool.entries.lock().unwrap();
        if Arc::strong_count(&self.lock) == 1 {
            entries.remove(&self.key);
        }
    }
}

pub struct DatasetCache<C, H> {
    clock: C,
    ttl: f64,
    capacity: usize,
    max_bytes: usize,
    weight: fn(&H) -> usize,
    datasets: Mutex<IndexMap<DatasetKey, Cached<H>>>,
    locks: KeyedLocks<DatasetKey>,
}
impl<C: Clock, H> DatasetCache<C, H> {
    pub fn new(
        clock: C,
        main_cache_ttl_seconds: f64,
        dataset_cache_entries: usize,
    ) -> Result<Self, CatalogError> {
        if !main_cache_ttl_seconds.is_finite() || main_cache_ttl_seconds < 0. {
            return Err(CatalogError::new(
                "main_cache_ttl_seconds must not be negative",
            ));
        }
        Ok(Self {
            clock,
            ttl: main_cache_ttl_seconds,
            capacity: dataset_cache_entries,
            max_bytes: usize::MAX,
            weight: |_| 0,
            datasets: Mutex::new(IndexMap::new()),
            locks: KeyedLocks {
                entries: Mutex::new(HashMap::new()),
            },
        })
    }
    /// Bound retained handles by entry count, caller-owned handle bytes and key storage.
    /// Handles larger than the entire cache can be used but are never retained.
    pub fn with_byte_limit(mut self, max_bytes: usize, weight: fn(&H) -> usize) -> Self {
        self.max_bytes = max_bytes;
        self.weight = weight;
        self
    }
    pub fn with_defaults(clock: C) -> Self {
        Self::new(
            clock,
            DEFAULT_MAIN_TTL_SECONDS,
            DEFAULT_DATASET_CACHE_ENTRIES,
        )
        .unwrap()
    }
    /// Errors from resolve/open propagate unchanged. An expired main dataset is
    /// never returned on failure. TTL starts before resolution, as in Python.
    pub fn open<S, E>(
        &self,
        key: DatasetKey,
        resolve: impl FnOnce() -> Result<Resolved<S>, E>,
        open: impl FnOnce(S) -> Result<H, E>,
    ) -> Result<OpenedDataset<H>, E> {
        if let Some(hit) = self.cached(&key, self.clock.now()) {
            return Ok(hit);
        }
        let entry = self.locks.entry(&key);
        let _guard = entry.lock.lock().unwrap();
        let now = self.clock.now();
        if let Some(hit) = self.cached(&key, now) {
            return Ok(hit);
        }
        let expired = if key.snapshot_id.is_none() && self.ttl != 0. {
            self.datasets
                .lock()
                .unwrap()
                .get(&key)
                .map(|c| c.opened.clone())
        } else {
            None
        };
        let resolved = resolve()?;
        let expires_at = if key.snapshot_id.is_some() {
            f64::INFINITY
        } else {
            now + self.ttl
        };
        if let Some(mut previous) = expired.filter(|p| p.snapshot_id == resolved.snapshot_id) {
            self.store(key, previous.clone(), expires_at);
            previous.cache_status = CacheStatus::Revalidated;
            return Ok(previous);
        }
        let dataset = open(resolved.session)?;
        let opened = OpenedDataset {
            dataset: Arc::new(dataset),
            snapshot_id: resolved.snapshot_id,
            cache_status: CacheStatus::Miss,
        };
        self.store(key, opened.clone(), expires_at);
        Ok(opened)
    }
    fn cached(&self, key: &DatasetKey, now: f64) -> Option<OpenedDataset<H>> {
        let mut datasets = self.datasets.lock().unwrap();
        if !datasets.get(key).is_some_and(|c| c.expires_at > now) {
            return None;
        }
        let cached = datasets.shift_remove(key).unwrap();
        let mut opened = cached.opened.clone();
        opened.cache_status = CacheStatus::Hit;
        datasets.insert(key.clone(), cached);
        Some(opened)
    }
    fn store(&self, key: DatasetKey, opened: OpenedDataset<H>, expires_at: f64) {
        if self.capacity == 0 {
            return;
        }
        let mut datasets = self.datasets.lock().unwrap();
        datasets.shift_remove(&key);
        let location_bytes = match &key.location {
            RepositoryLocation::Http(h) => h.url.capacity(),
            RepositoryLocation::S3(s) => s
                .bucket
                .capacity()
                .saturating_add(s.prefix.capacity())
                .saturating_add(s.region.capacity()),
        };
        let bytes = (self.weight)(&opened.dataset)
            .saturating_add(
                std::mem::size_of::<DatasetKey>() + std::mem::size_of::<Cached<H>>() + 128,
            )
            .saturating_add(key.collection_id.capacity())
            .saturating_add(key.snapshot_id.as_ref().map_or(0, String::capacity))
            .saturating_add(location_bytes)
            .saturating_add(
                key.containers
                    .capacity()
                    .saturating_mul(std::mem::size_of::<VirtualChunkContainer>()),
            )
            .saturating_add(key.containers.iter().fold(0usize, |bytes, container| {
                bytes
                    .saturating_add(container.kind.capacity())
                    .saturating_add(container.url_prefix.capacity())
            }))
            .saturating_add(opened.snapshot_id.capacity());
        if bytes > self.max_bytes {
            return;
        }
        datasets.insert(
            key,
            Cached {
                opened,
                expires_at,
                bytes,
            },
        );
        while datasets.len() > self.capacity
            || datasets.values().fold(0u128, |n, c| n + c.bytes as u128) > self.max_bytes as u128
        {
            datasets.shift_remove_index(0);
        }
    }
    pub fn len(&self) -> usize {
        self.datasets.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn metadata_bytes(&self) -> usize {
        self.datasets
            .lock()
            .unwrap()
            .values()
            .map(|c| c.bytes)
            .sum()
    }
    pub fn lock_entries(&self) -> usize {
        self.locks.entries.lock().unwrap().len()
    }
}

/// Python keeps repositories for the process lifetime, independently of the
/// bounded dataset LRU. This optional opaque-handle cache has the same key and
/// keyed single-flight semantics; callers supply repository construction.
pub struct RepositoryCache<H> {
    repositories: Mutex<HashMap<RepositoryKey, Arc<H>>>,
    locks: KeyedLocks<RepositoryKey>,
}
impl<H> Default for RepositoryCache<H> {
    fn default() -> Self {
        Self {
            repositories: Mutex::new(HashMap::new()),
            locks: KeyedLocks {
                entries: Mutex::new(HashMap::new()),
            },
        }
    }
}
impl<H> RepositoryCache<H> {
    pub fn get_or_open<E>(
        &self,
        key: RepositoryKey,
        open: impl FnOnce() -> Result<H, E>,
    ) -> Result<Arc<H>, E> {
        if let Some(hit) = self.repositories.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        let entry = self.locks.entry(&key);
        let _guard = entry.lock.lock().unwrap();
        if let Some(hit) = self.repositories.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        let opened = Arc::new(open()?);
        self.repositories
            .lock()
            .unwrap()
            .insert(key, opened.clone());
        Ok(opened)
    }
}
