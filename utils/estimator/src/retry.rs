use anyhow::{bail, Result};
use std::future::Future;
use std::time::Duration;

/// Bound an idempotent network future with a timeout. On timeout returns an error
/// classified `transient` downstream. Lifted from `fault-proof/src/prover.rs:341-368`.
pub async fn network_call_with_timeout<F, T>(
    timeout_secs: u64,
    operation: &str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => {
            tracing::warn!(operation, error = %e, "network error");
            Err(e)
        }
        Err(_) => {
            tracing::warn!(operation, timeout_secs, "network call timed out");
            bail!("network timeout after {timeout_secs}s for {operation}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_value_when_future_completes() {
        let v = network_call_with_timeout(5, "noop", async { Ok::<_, anyhow::Error>(7) })
            .await
            .unwrap();
        assert_eq!(v, 7);
    }

    #[tokio::test]
    async fn times_out_slow_future() {
        let res: Result<()> = network_call_with_timeout(1, "slow", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await;
        assert!(res.is_err());
        assert!(format!("{}", res.unwrap_err()).contains("timeout"));
    }
}
