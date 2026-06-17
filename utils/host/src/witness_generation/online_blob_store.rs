use alloy_consensus::Blob;
use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use kona_derive::{BlobProvider, PipelineError, PipelineErrorKind};
use kona_protocol::BlockInfo;
use kzg_rs::{Blob as KzgRsBlob, Bytes48};
use op_succinct_client_utils::witness::BlobData;
use std::{
    fmt,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub struct OnlineBlobStore<T: BlobProvider> {
    pub provider: T,
    pub store: Arc<Mutex<BlobData>>,
}

/// Error type for [`OnlineBlobStore`].
///
/// Wraps either an error from the inner [`BlobProvider`] (`E`) or a local failure
/// while computing/recording KZG data (poisoned lock, malformed blob, KZG fault).
/// A local failure becomes a retryable error rather than a process crash.
#[derive(Debug)]
pub enum OnlineBlobStoreError<E> {
    /// An error from the wrapped inner [`BlobProvider`].
    Inner(E),
    /// A failure while computing or recording KZG data for a fetched blob.
    Kzg(anyhow::Error),
}

impl<E: fmt::Display> fmt::Display for OnlineBlobStoreError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(e) => write!(f, "{e}"),
            Self::Kzg(e) => write!(f, "online blob store KZG error: {e}"),
        }
    }
}

impl<E> std::error::Error for OnlineBlobStoreError<E> where E: fmt::Debug + fmt::Display {}

// The kona `BlobProvider::Error` bound requires `Into<PipelineErrorKind>`. Convert
// inner errors using their own conversion, and KZG faults into a critical provider
// error.
impl<E> From<OnlineBlobStoreError<E>> for PipelineErrorKind
where
    E: Into<PipelineErrorKind>,
{
    fn from(err: OnlineBlobStoreError<E>) -> Self {
        match err {
            OnlineBlobStoreError::Inner(e) => e.into(),
            OnlineBlobStoreError::Kzg(e) => PipelineError::Provider(e.to_string()).crit(),
        }
    }
}

/// This is only invoked when fetching the blobs in online mode. In zkVM mode,
/// the blobs are given upfront.
#[async_trait]
impl<T: BlobProvider + Send> BlobProvider for OnlineBlobStore<T> {
    type Error = OnlineBlobStoreError<T::Error>;

    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        let blobs = self
            .provider
            .get_and_validate_blobs(block_ref, blob_hashes)
            .await
            .map_err(OnlineBlobStoreError::Inner)?;
        let settings = EnvKzgSettings::default();

        let mut store = self.store.lock().map_err(|e| {
            OnlineBlobStoreError::Kzg(anyhow::anyhow!("blob store mutex poisoned: {e}"))
        })?;
        for blob in &blobs {
            let (c_kzg_blob, commitment, proof) =
                get_blob_data(blob, &settings).map_err(OnlineBlobStoreError::Kzg)?;

            store.blobs.push(c_kzg_blob);
            store.commitments.push(commitment);
            store.proofs.push(proof);
        }
        Ok(blobs)
    }
}

/// Get the blob data for the given blob with the given settings.
///
/// Returns an error (rather than panicking) if the blob is malformed or any KZG
/// operation fails, so a fault becomes a retryable error.
fn get_blob_data(blob: &Blob, settings: &EnvKzgSettings) -> Result<(KzgRsBlob, Bytes48, Bytes48)> {
    let c_kzg_blob = c_kzg::Blob::from_bytes(blob.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid KZG blob: {e:?}"))?;
    let commitment = settings
        .get()
        .blob_to_kzg_commitment(&c_kzg_blob)
        .map_err(|e| anyhow::anyhow!("blob_to_kzg_commitment failed: {e:?}"))?;
    let proof = settings
        .get()
        .compute_blob_kzg_proof(&c_kzg_blob, &commitment.to_bytes())
        .map_err(|e| anyhow::anyhow!("compute_blob_kzg_proof failed: {e:?}"))?;
    let rs_blob = KzgRsBlob::from_slice(&*c_kzg_blob)
        .map_err(|e| anyhow::anyhow!("KzgRsBlob::from_slice failed: {e:?}"))?;
    Ok((rs_blob, Bytes48(*commitment.to_bytes()), Bytes48(*proof.to_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed (wrong-sized) blob must produce an error rather than panicking.
    #[test]
    fn get_blob_data_rejects_malformed_blob_without_panicking() {
        let settings = EnvKzgSettings::default();
        // `Blob::default()` is all zeros, which is a valid KZG blob. Build an
        // intentionally malformed blob by corrupting a field element so its high
        // bits exceed the BLS field modulus, which `c_kzg::Blob::from_bytes` /
        // commitment computation must reject.
        let mut blob = Blob::default();
        // Set the first 32-byte field element to all 0xff, which is not a valid
        // canonical field element (>= the BLS12-381 scalar field modulus).
        for byte in blob.iter_mut().take(32) {
            *byte = 0xff;
        }

        let result = get_blob_data(&blob, &settings);
        assert!(result.is_err(), "expected Err for malformed blob, got Ok");
    }
}
