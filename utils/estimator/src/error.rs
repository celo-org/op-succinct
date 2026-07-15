use sp1_core_executor::ExecutionError;
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
    /// SP1 zkVM execute failure (`sp1_core_executor::ExecutionError`). Execute is
    /// deterministic over fixed stdin, so every variant — the memory limit (`TooMuchMemory`),
    /// cycle-limit overruns, program faults — reproduces on retry and is therefore never
    /// retried. The concrete error is carried through for logging. A real host/cgroup OOM
    /// SIGKILLs the process instead and never surfaces here.
    #[error("SP1 execute failed: {0}")]
    Sp1Execute(#[from] ExecutionError),
    #[error("transient failure: {0}")]
    Transient(#[source] anyhow::Error),
    #[error("fatal failure: {0}")]
    Fatal(#[source] anyhow::Error),
}

impl EstimatorError {
    /// True if the failure is worth retrying. SP1 execute failures (`Sp1Execute`) are never
    /// retried — execute is deterministic, so a retry reproduces the same error.
    pub fn is_transient(&self) -> bool {
        match self {
            EstimatorError::NoHealthyBackend |
            EstimatorError::NoStateAvailable |
            EstimatorError::MissingTrieNode |
            EstimatorError::DnsLookupFailure |
            EstimatorError::Transient(_) => true,
            EstimatorError::ExceedsProofWindow |
            EstimatorError::Sp1Execute(_) |
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
    fn sp1_execute_errors_are_never_transient() {
        // Execute is deterministic over fixed stdin, so every variant is fatal — the memory
        // limit and a cycle-limit overrun alike.
        assert!(!EstimatorError::Sp1Execute(ExecutionError::TooMuchMemory()).is_transient());
        assert!(!EstimatorError::Sp1Execute(ExecutionError::ExceededCycleLimit(1_000_000))
            .is_transient());
    }

    #[test]
    fn missing_trie_node_is_transient() {
        let e = EstimatorError::classify(anyhow::anyhow!(
            "server returned an error response: error code -32000: missing trie node"
        ));
        assert!(matches!(e, EstimatorError::MissingTrieNode));
        assert!(e.is_transient());
    }

    #[test]
    fn unknown_error_is_transient_by_default() {
        let e = EstimatorError::classify(anyhow::anyhow!("some novel rpc blip"));
        assert!(matches!(e, EstimatorError::Transient(_)));
        assert!(e.is_transient());
    }
}
