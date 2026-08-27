use anyhow::Result;
use rkyv::rancor::Error as RkyvError;
use sp1_sdk::SP1Stdin;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// DA-type discriminator folded into every cache key. The on-disk `WitnessData`
/// layouts differ by DA (EigenDA adds `eigenda_data`), so a key without this would
/// let an EigenDA blob be loaded as a DefaultWitnessData and vice-versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaType {
    Ethereum,
    Celestia,
    EigenDa,
}

impl DaType {
    pub fn as_str(self) -> &'static str {
        match self {
            DaType::Ethereum => "ethereum",
            DaType::Celestia => "celestia",
            DaType::EigenDa => "eigenda",
        }
    }
}

/// On-disk cache for `WitnessData` and `SP1Stdin` blobs, keyed by
/// `(chain_id, start, end, da_type)` under an absolute base directory.
#[derive(Debug, Clone)]
pub struct WitnessCache {
    base_dir: PathBuf,
    chain_id: u64,
    da_type: DaType,
}

impl WitnessCache {
    pub fn new(base_dir: impl AsRef<Path>, chain_id: u64, da_type: DaType) -> Self {
        Self { base_dir: base_dir.as_ref().to_path_buf(), chain_id, da_type }
    }

    fn cache_dir(&self) -> PathBuf {
        self.base_dir.join(self.chain_id.to_string()).join("witness-cache")
    }

    pub fn witness_path(&self, start: u64, end: u64) -> PathBuf {
        self.cache_dir().join(format!("{start}-{end}-{}-witness.bin", self.da_type.as_str()))
    }

    pub fn stdin_path(&self, start: u64, end: u64) -> PathBuf {
        self.cache_dir().join(format!("{start}-{end}-{}-stdin.bin", self.da_type.as_str()))
    }

    /// A process-unique temp path sibling to `final_path`. Concurrent writers (the
    /// pipeline and the executor building the same range) each write their own temp,
    /// then atomically rename onto the shared final path — so they never collide on a
    /// half-written tmp file (a deterministic tmp name would race).
    fn unique_tmp(final_path: &Path) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        final_path.with_extension(format!("tmp.{pid}.{n}"))
    }
}

impl WitnessCache {
    pub fn has_stdin(&self, start: u64, end: u64) -> bool {
        self.stdin_path(start, end).exists()
    }

    pub fn save_stdin(&self, start: u64, end: u64, stdin: &SP1Stdin) -> Result<PathBuf> {
        let dir = self.cache_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = self.stdin_path(start, end);
        let tmp = Self::unique_tmp(&path);
        fs::write(&tmp, bincode::serialize(stdin)?)?;
        fs::rename(&tmp, &path)?; // atomic publish so a crash mid-write leaves no half blob
        Ok(path)
    }

    pub fn load_stdin(&self, start: u64, end: u64) -> Result<Option<SP1Stdin>> {
        let path = self.stdin_path(start, end);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(bincode::deserialize(&bytes)?))
    }
}

impl WitnessCache {
    pub fn has_witness(&self, start: u64, end: u64) -> bool {
        self.witness_path(start, end).exists()
    }

    /// Persist a `WitnessData` blob via rkyv. `W` is the DA-specific witness type.
    pub fn save_witness<W>(&self, start: u64, end: u64, witness: &W) -> Result<PathBuf>
    where
        W: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        >,
    {
        let dir = self.cache_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = self.witness_path(start, end);
        let tmp = Self::unique_tmp(&path);
        let bytes = rkyv::to_bytes::<RkyvError>(witness)?;
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?; // atomic publish so a crash mid-write leaves no half blob
        Ok(path)
    }

    /// Load a `WitnessData` blob via rkyv. Returns `None` on a miss.
    pub fn load_witness<W>(&self, start: u64, end: u64) -> Result<Option<W>>
    where
        W: rkyv::Archive,
        W::Archived: rkyv::Deserialize<W, rkyv::api::high::HighDeserializer<RkyvError>>
            + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
    {
        let path = self.witness_path(start, end);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(rkyv::from_bytes::<W, RkyvError>(&bytes)?))
    }

    /// Drop the (large) `WitnessData` blob once its stdin is built. Idempotent.
    pub fn drop_witness(&self, start: u64, end: u64) -> Result<()> {
        let path = self.witness_path(start, end);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

impl WitnessCache {
    /// The proof cache: `ExecutionStats` keyed by range. Execute results are pure functions
    /// of the range, so a cached result is reusable across game retries, restarts, and by a
    /// game whose ranges were executed speculatively ahead of its discovery. JSON for
    /// operator inspectability; the blobs are a few hundred bytes.
    pub fn stats_path(&self, start: u64, end: u64) -> PathBuf {
        self.cache_dir().join(format!("{start}-{end}-{}-stats.json", self.da_type.as_str()))
    }

    pub fn has_stats(&self, start: u64, end: u64) -> bool {
        self.stats_path(start, end).exists()
    }

    pub fn save_stats(
        &self,
        start: u64,
        end: u64,
        stats: &op_succinct_host_utils::stats::ExecutionStats,
    ) -> Result<PathBuf> {
        let dir = self.cache_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = self.stats_path(start, end);
        let tmp = Self::unique_tmp(&path);
        fs::write(&tmp, serde_json::to_vec(stats)?)?;
        fs::rename(&tmp, &path)?; // atomic publish so a crash mid-write leaves no half blob
        Ok(path)
    }

    pub fn load_stats(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Option<op_succinct_host_utils::stats::ExecutionStats>> {
        let path = self.stats_path(start, end);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(&path)?)?))
    }
}

impl WitnessCache {
    /// Delete a range's stdin blob (called after the owning game succeeds + grace).
    pub fn prune_stdin(&self, start: u64, end: u64) -> Result<()> {
        let path = self.stdin_path(start, end);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Age in seconds of a range's stdin blob, or `None` if absent/unreadable.
    pub fn stdin_age_secs(&self, start: u64, end: u64, now: std::time::SystemTime) -> Option<u64> {
        let path = self.stdin_path(start, end);
        let modified = fs::metadata(&path).ok()?.modified().ok()?;
        now.duration_since(modified).ok().map(|d| d.as_secs())
    }
}

/// Summary of one [`WitnessCache::enforce_size_cap`] sweep, for logging.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CapSweepOutcome {
    /// Total bytes of cache blobs (excluding stray temps) before eviction.
    pub bytes_before: u64,
    /// Bytes reclaimed by this sweep (evicted blobs + stray temps).
    pub bytes_freed: u64,
    /// Number of whole blobs evicted to fit the cap.
    pub files_deleted: usize,
    /// Number of stray `*.tmp.*` files reaped (always removed, cap or not).
    pub tmp_deleted: usize,
}

/// Parse the leading `{start}-{end}` block key from a cache file name (e.g.
/// `10452239-10452439-eigenda-stdin.bin` → `(10452239, 10452439)`). Returns `None`
/// for a name that does not begin with two dash-separated integers.
fn parse_range_key(name: &str) -> Option<(u64, u64)> {
    let mut parts = name.split('-');
    let start = parts.next()?.parse().ok()?;
    let end = parts.next()?.parse().ok()?;
    Some((start, end))
}

impl WitnessCache {
    /// Bound the on-disk cache to `max_bytes`. This is the backstop garbage collector for
    /// blobs the per-game prune misses: orphans from a `Fatal`/`WrongType` outcome, prebuilt
    /// ranges no game ever executed, and — crucially — everything whose in-memory prune was
    /// lost when the process was replaced (the prune list is not persisted, so a pod swap
    /// leaks every not-yet-pruned blob).
    ///
    /// Two phases:
    /// 1. Always reap stray `*.tmp.*` files — leftovers from an interrupted or failed atomic
    ///    write (e.g. an ENOSPC crash between `write` and `rename`); never useful.
    /// 2. If the cache still exceeds `max_bytes`, evict whole blobs **oldest-first** (by
    ///    mtime) until it fits, skipping any file whose `(start, end)` range is in
    ///    `protected` — the ranges of games still awaiting a background retry, whose cached
    ///    stdin we keep so the retry stays a cache hit.
    ///
    /// A protected set larger than the cap simply leaves the cache above `max_bytes` rather
    /// than deleting data a pending retry needs.
    pub fn enforce_size_cap(
        &self,
        max_bytes: u64,
        protected: &HashSet<(u64, u64)>,
    ) -> Result<CapSweepOutcome> {
        let dir = self.cache_dir();
        let read_dir = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            // A cache that was never created is trivially within budget.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CapSweepOutcome::default())
            }
            Err(e) => return Err(e.into()),
        };

        struct Blob {
            path: PathBuf,
            size: u64,
            mtime: SystemTime,
            key: Option<(u64, u64)>,
        }
        let mut blobs: Vec<Blob> = Vec::new();
        let mut outcome = CapSweepOutcome::default();

        for entry in read_dir.flatten() {
            let meta = match entry.metadata() {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let size = meta.len();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Phase 1: stray temp from a failed/abandoned atomic write — always drop.
            if name.contains(".tmp.") {
                if fs::remove_file(entry.path()).is_ok() {
                    outcome.tmp_deleted += 1;
                    outcome.bytes_freed += size;
                }
                continue;
            }
            outcome.bytes_before += size;
            blobs.push(Blob {
                path: entry.path(),
                size,
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                key: parse_range_key(&name),
            });
        }

        let mut total = outcome.bytes_before;
        if total <= max_bytes {
            return Ok(outcome);
        }

        // Phase 2: evict oldest-first until within budget, skipping protected ranges.
        blobs.sort_by_key(|b| b.mtime);
        for blob in blobs {
            if total <= max_bytes {
                break;
            }
            if let Some(key) = blob.key {
                if protected.contains(&key) {
                    continue;
                }
            }
            if fs::remove_file(&blob.path).is_ok() {
                total = total.saturating_sub(blob.size);
                outcome.bytes_freed += blob.size;
                outcome.files_deleted += 1;
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_include_da_type_and_absolute_base() {
        let cache = WitnessCache::new("/var/op-succinct/cache", 42220, DaType::EigenDa);
        let w = cache.witness_path(100, 200);
        let s = cache.stdin_path(100, 200);
        assert_eq!(
            w,
            PathBuf::from("/var/op-succinct/cache/42220/witness-cache/100-200-eigenda-witness.bin")
        );
        assert_eq!(
            s,
            PathBuf::from("/var/op-succinct/cache/42220/witness-cache/100-200-eigenda-stdin.bin")
        );
    }

    #[test]
    fn eigenda_and_ethereum_keys_differ() {
        let eigen = WitnessCache::new("/c", 1, DaType::EigenDa);
        let eth = WitnessCache::new("/c", 1, DaType::Ethereum);
        assert_ne!(eigen.witness_path(1, 2), eth.witness_path(1, 2));
    }

    #[test]
    fn stdin_round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
        assert!(!cache.has_stdin(10, 20));
        let stdin = sp1_sdk::SP1Stdin::default();
        cache.save_stdin(10, 20, &stdin).unwrap();
        assert!(cache.has_stdin(10, 20));
        let loaded = cache.load_stdin(10, 20).unwrap();
        assert!(loaded.is_some());
    }

    #[test]
    fn stats_round_trip_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
        assert!(!cache.has_stats(10, 20));
        assert!(cache.load_stats(10, 20).unwrap().is_none());
        let stats = op_succinct_host_utils::stats::ExecutionStats {
            batch_start: 10,
            batch_end: 20,
            total_sp1_gas: 123_456,
            l1_fees: u128::from(u64::MAX) + 1, // u128 fields must survive JSON
            ..Default::default()
        };
        cache.save_stats(10, 20, &stats).unwrap();
        assert!(cache.has_stats(10, 20));
        assert_eq!(cache.load_stats(10, 20).unwrap(), Some(stats));
    }

    #[test]
    fn repeated_stdin_saves_leave_one_clean_blob_no_stray_temps() {
        // Two saves to the same range (the pipeline-vs-executor race) must each succeed
        // via their own unique temp and leave exactly one valid final blob, no leftovers.
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        assert!(cache.has_stdin(1, 2));
        assert!(cache.load_stdin(1, 2).unwrap().is_some());
        let cache_dir = dir.path().join("7").join("witness-cache");
        let strays: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(strays.is_empty(), "stray tmp files left behind: {strays:?}");
    }

    #[test]
    fn load_stdin_returns_none_on_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        assert!(cache.load_stdin(1, 2).unwrap().is_none());
    }

    #[test]
    fn prune_stdin_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        assert!(cache.has_stdin(1, 2));
        cache.prune_stdin(1, 2).unwrap();
        assert!(!cache.has_stdin(1, 2));
        cache.prune_stdin(1, 2).unwrap(); // no error on missing
    }

    #[test]
    fn stdin_age_is_some_after_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        let age = cache.stdin_age_secs(1, 2, std::time::SystemTime::now());
        assert!(age.is_some());
        assert!(cache.stdin_age_secs(9, 9, std::time::SystemTime::now()).is_none());
    }

    fn set_mtime(path: &Path, secs: u64) {
        use std::time::Duration;
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    fn blob_size(cache: &WitnessCache, start: u64, end: u64) -> u64 {
        fs::metadata(cache.stdin_path(start, end)).unwrap().len()
    }

    #[test]
    fn parse_range_key_reads_leading_blocks() {
        assert_eq!(parse_range_key("10452239-10452439-eigenda-stdin.bin"), Some((10452239, 10452439)));
        assert_eq!(parse_range_key("100-200-eigenda-witness.tmp.7.3"), Some((100, 200)));
        assert_eq!(parse_range_key("memory_model.json"), None);
    }

    #[test]
    fn size_cap_evicts_oldest_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        for (s, e) in [(1, 2), (2, 3), (3, 4)] {
            cache.save_stdin(s, e, &sp1_sdk::SP1Stdin::default()).unwrap();
        }
        set_mtime(&cache.stdin_path(1, 2), 100); // oldest
        set_mtime(&cache.stdin_path(2, 3), 200);
        set_mtime(&cache.stdin_path(3, 4), 300); // newest
        let size = blob_size(&cache, 1, 2);

        // Room for two of the three blobs → the single oldest is evicted.
        let outcome = cache.enforce_size_cap(2 * size, &HashSet::new()).unwrap();

        assert_eq!(outcome.files_deleted, 1);
        assert_eq!(outcome.bytes_freed, size);
        assert!(!cache.has_stdin(1, 2), "oldest should be evicted");
        assert!(cache.has_stdin(2, 3));
        assert!(cache.has_stdin(3, 4));
    }

    #[test]
    fn size_cap_skips_protected_ranges() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        for (s, e) in [(1, 2), (2, 3), (3, 4)] {
            cache.save_stdin(s, e, &sp1_sdk::SP1Stdin::default()).unwrap();
        }
        set_mtime(&cache.stdin_path(1, 2), 100); // oldest, but protected
        set_mtime(&cache.stdin_path(2, 3), 200);
        set_mtime(&cache.stdin_path(3, 4), 300);
        let size = blob_size(&cache, 1, 2);

        // Cap fits one blob; the oldest is protected, so the next-oldest go instead.
        let protected = HashSet::from([(1u64, 2u64)]);
        cache.enforce_size_cap(size, &protected).unwrap();

        assert!(cache.has_stdin(1, 2), "protected range must survive even though oldest");
        assert!(!cache.has_stdin(2, 3));
        assert!(!cache.has_stdin(3, 4));
    }

    #[test]
    fn size_cap_reaps_stray_temps_regardless_of_budget() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        let cache_dir = cache.stdin_path(1, 2).parent().unwrap().to_path_buf();
        let stray = cache_dir.join("5-6-eigenda-stdin.tmp.999.0");
        fs::write(&stray, b"garbage").unwrap();

        let outcome = cache.enforce_size_cap(u64::MAX, &HashSet::new()).unwrap();

        assert_eq!(outcome.tmp_deleted, 1);
        assert_eq!(outcome.files_deleted, 0, "under budget: no blob eviction");
        assert!(!stray.exists());
        assert!(cache.has_stdin(1, 2));
    }

    #[test]
    fn size_cap_is_noop_under_budget() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        let outcome = cache.enforce_size_cap(u64::MAX, &HashSet::new()).unwrap();
        assert_eq!(outcome.files_deleted, 0);
        assert_eq!(outcome.bytes_freed, 0);
        assert!(outcome.bytes_before > 0);
        assert!(cache.has_stdin(1, 2));
    }

    #[test]
    fn size_cap_on_missing_dir_is_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 7, DaType::EigenDa);
        let outcome = cache.enforce_size_cap(0, &HashSet::new()).unwrap();
        assert_eq!(outcome, CapSweepOutcome::default());
    }

    #[cfg(feature = "eigenda")]
    #[test]
    fn witness_round_trips_and_drops() {
        use op_succinct_client_utils::witness::EigenDAWitnessData;
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
        let witness = EigenDAWitnessData::default();
        assert!(!cache.has_witness(10, 20));
        cache.save_witness(10, 20, &witness).unwrap();
        assert!(cache.has_witness(10, 20));
        let loaded: Option<EigenDAWitnessData> = cache.load_witness(10, 20).unwrap();
        assert!(loaded.is_some());
        cache.drop_witness(10, 20).unwrap();
        assert!(!cache.has_witness(10, 20));
        cache.drop_witness(10, 20).unwrap(); // idempotent
    }
}
