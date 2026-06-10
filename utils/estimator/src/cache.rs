use anyhow::Result;
use rkyv::rancor::Error as RkyvError;
use sp1_sdk::SP1Stdin;
use std::fs;
use std::path::{Path, PathBuf};

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
        let tmp = path.with_extension("bin.tmp");
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
        let tmp = path.with_extension("bin.tmp");
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
    fn load_stdin_returns_none_on_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        assert!(cache.load_stdin(1, 2).unwrap().is_none());
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
