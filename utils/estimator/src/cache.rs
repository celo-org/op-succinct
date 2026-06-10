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
}
