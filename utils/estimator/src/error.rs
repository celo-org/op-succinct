use thiserror::Error;

#[derive(Debug, Error)]
pub enum EstimatorError {
    #[error("no backend is currently healthy to serve traffic")]
    NoHealthyBackend,
    #[error("no state available for block")]
    NoStateAvailable,
    #[error("distance to target block exceeds maximum proof window")]
    ExceedsProofWindow,
    #[error("missing trie node")]
    MissingTrieNode,
    #[error("dns lookup failure")]
    DnsLookupFailure,
    #[error("out of memory during execution")]
    Oom,
    #[error("transient failure: {0}")]
    Transient(#[source] anyhow::Error),
    #[error("fatal failure: {0}")]
    Fatal(#[source] anyhow::Error),
}

impl EstimatorError {
    /// True if the failure is worth retrying. `Oom` is never retried.
    pub fn is_transient(&self) -> bool {
        match self {
            EstimatorError::NoHealthyBackend |
            EstimatorError::NoStateAvailable |
            EstimatorError::DnsLookupFailure |
            EstimatorError::Transient(_) => true,
            EstimatorError::ExceedsProofWindow |
            EstimatorError::MissingTrieNode |
            EstimatorError::Oom |
            EstimatorError::Fatal(_) => false,
        }
    }

    /// Classify an upstream `anyhow` error by matching the legacy failure-pattern
    /// substrings against its full chain (mirrors the monitor's FAILURE_PATTERNS).
    pub fn classify(err: anyhow::Error) -> Self {
        let msg = format!("{err:#}").to_lowercase();
        if msg.contains("no backend is currently healthy to serve traffic") {
            EstimatorError::NoHealthyBackend
        } else if msg.contains("no state available for block") {
            EstimatorError::NoStateAvailable
        } else if msg.contains("distance to target block exceeds maximum proof window") {
            EstimatorError::ExceedsProofWindow
        } else if msg.contains("missing trie node") {
            EstimatorError::MissingTrieNode
        } else if msg.contains("dns error") || msg.contains("failed to fetch safe head") {
            EstimatorError::DnsLookupFailure
        } else if msg.contains("memory allocation")
            || msg.contains("out of memory")
            // SP1's in-VM memory-limit error: `ExecutionError::TooMuchMemory` renders as
            // "SP1 program consumes too much memory". Catch it so an OOM is never retried.
            || msg.contains("too much memory")
        {
            EstimatorError::Oom
        } else {
            EstimatorError::Transient(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_legacy_failure_patterns() {
        let e = EstimatorError::classify(anyhow::anyhow!(
            "no backend is currently healthy to serve traffic"
        ));
        assert!(matches!(e, EstimatorError::NoHealthyBackend));
        assert!(e.is_transient());
    }

    #[test]
    fn proof_window_is_fatal() {
        let e = EstimatorError::classify(anyhow::anyhow!(
            "distance to target block exceeds maximum proof window"
        ));
        assert!(matches!(e, EstimatorError::ExceedsProofWindow));
        assert!(!e.is_transient());
    }

    #[test]
    fn oom_is_never_transient() {
        assert!(!EstimatorError::Oom.is_transient());
    }

    #[test]
    fn sp1_too_much_memory_classifies_as_oom() {
        // SP1's `ExecutionError::TooMuchMemory` renders as this message.
        let e = EstimatorError::classify(anyhow::anyhow!(
            "SP1 execute failed: SP1 program consumes too much memory"
        ));
        assert!(matches!(e, EstimatorError::Oom));
        assert!(!e.is_transient());
    }

    #[test]
    fn unknown_error_is_transient_by_default() {
        let e = EstimatorError::classify(anyhow::anyhow!("some novel rpc blip"));
        assert!(matches!(e, EstimatorError::Transient(_)));
        assert!(e.is_transient());
    }
}
