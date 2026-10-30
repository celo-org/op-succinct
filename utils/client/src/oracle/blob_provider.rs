use crate::witness::BlobData;
use alloc::{boxed::Box, vec::Vec};
use alloy_consensus::Blob;
use alloy_eips::eip4844::kzg_to_versioned_hash;
use alloy_primitives::B256;
use async_trait::async_trait;
use kona_derive::{BlobProvider, BlobProviderError};
use kona_protocol::BlockInfo;
use kzg_rs::{get_g2_points, get_roots_of_unity, KzgSettings};

/// Trusted-setup tables for blob KZG proof verification.
///
/// kzg-rs decodes each table into a static on first use, and in the zkVM decoding the G1 points
/// costs millions of cycles. Blob verification in kzg-rs 0.2.9 reads only the roots of unity and
/// `g2_points[1]`; G1 is needed to compute commitments and proofs, which the guest never does, so
/// it is left empty. If a kzg-rs upgrade starts indexing G1 here, the empty slice panics: the
/// guest fails to prove, but never accepts a bad proof. The tests below pin the expected outcomes
/// with this setup.
fn blob_verification_settings() -> KzgSettings {
    KzgSettings { roots_of_unity: get_roots_of_unity(), g1_points: &[], g2_points: get_g2_points() }
}

#[derive(Clone, Debug, Default)]
pub struct BlobStore {
    versioned_blobs: Vec<(B256, Blob)>,
}

impl From<BlobData> for BlobStore {
    fn from(value: BlobData) -> Self {
        let blobs: Vec<_> =
            value.blobs.iter().map(|b| kzg_rs::Blob::from_slice(&b.0).unwrap()).collect();
        let versioned_blobs = value
            .commitments
            .iter()
            .map(|c| kzg_to_versioned_hash(c.as_slice()))
            .zip(blobs.iter().map(|b| Blob::from(b.0)))
            .rev()
            .collect();

        // With nothing to verify, skip loading the trusted setup. Mismatched lengths still go
        // through kzg-rs, which rejects them.
        let nothing_to_verify =
            blobs.is_empty() && value.commitments.is_empty() && value.proofs.is_empty();
        if !nothing_to_verify {
            match kzg_rs::KzgProof::verify_blob_kzg_proof_batch(
                blobs,
                value.commitments,
                value.proofs,
                &blob_verification_settings(),
            ) {
                Ok(true) => {} // Verification passed
                Ok(false) => panic!("KZG proof verification failed: invalid proofs"),
                Err(e) => panic!("KZG proof verification error: {}", e),
            }
        }

        Self { versioned_blobs }
    }
}

#[async_trait]
impl BlobProvider for BlobStore {
    type Error = BlobProviderError;

    async fn get_and_validate_blobs(
        &mut self,
        _: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        Ok(blob_hashes
            .iter()
            .filter_map(|hash| {
                let (blob_hash, blob) = self.versioned_blobs.pop().unwrap();
                (*hash == blob_hash).then(|| Box::new(blob))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kzg_fixtures::{blob_fixture, BlobFixture};

    fn blob_data(fixtures: &[BlobFixture]) -> BlobData {
        BlobData {
            blobs: fixtures.iter().map(|f| f.blob.clone()).collect(),
            commitments: fixtures.iter().map(|f| f.commitment.clone()).collect(),
            proofs: fixtures.iter().map(|f| f.proof.clone()).collect(),
        }
    }

    /// Asserts the store hands out each fixture's blob under its versioned hash, in order.
    fn assert_serves(store: &BlobStore, fixtures: &[BlobFixture]) {
        let expected: Vec<_> = fixtures
            .iter()
            .map(|f| (kzg_to_versioned_hash(f.commitment.as_slice()), Blob::from(f.blob.0)))
            .rev()
            .collect();
        assert_eq!(store.versioned_blobs, expected);
    }

    // The settings blob verification uses: roots of unity and G2, no G1.

    #[test]
    fn blob_verification_settings_omit_g1() {
        let settings = blob_verification_settings();
        assert!(settings.g1_points.is_empty());
        assert!(!settings.roots_of_unity.is_empty());
        assert!(settings.g2_points.len() > 1);
    }

    // Valid blobs are accepted without G1, on both the single-blob and the batch path.

    #[test]
    fn empty_blob_data_is_accepted() {
        let store = BlobStore::from(BlobData::default());
        assert!(store.versioned_blobs.is_empty());
    }

    #[test]
    fn single_valid_blob_is_accepted() {
        let fixtures = [blob_fixture(1)];
        assert_serves(&BlobStore::from(blob_data(&fixtures)), &fixtures);
    }

    #[test]
    fn multiple_valid_blobs_are_accepted() {
        let fixtures = [blob_fixture(1), blob_fixture(2), blob_fixture(3)];
        assert_serves(&BlobStore::from(blob_data(&fixtures)), &fixtures);
    }

    // Well-formed but wrong proofs are rejected without G1.

    #[test]
    #[should_panic(expected = "KZG proof verification failed: invalid proofs")]
    fn single_blob_with_wrong_proof_is_rejected() {
        let mut data = blob_data(&[blob_fixture(1)]);
        data.proofs[0] = blob_fixture(2).proof;
        let _ = BlobStore::from(data);
    }

    #[test]
    #[should_panic(expected = "KZG proof verification failed: invalid proofs")]
    fn multiple_blobs_with_swapped_proofs_are_rejected() {
        let mut data = blob_data(&[blob_fixture(1), blob_fixture(2)]);
        data.proofs.swap(0, 1);
        let _ = BlobStore::from(data);
    }

    #[test]
    #[should_panic(expected = "KZG proof verification failed: invalid proofs")]
    fn multiple_blobs_with_wrong_commitment_are_rejected() {
        let mut data = blob_data(&[blob_fixture(1), blob_fixture(2)]);
        data.commitments[1] = blob_fixture(3).commitment;
        let _ = BlobStore::from(data);
    }

    // Mismatched lengths are rejected, including when the blob list is empty.

    #[test]
    #[should_panic(expected = "KZG proof verification error: Invalid commitments length")]
    fn missing_commitment_is_rejected() {
        let mut data = blob_data(&[blob_fixture(1), blob_fixture(2)]);
        data.commitments.pop();
        let _ = BlobStore::from(data);
    }

    #[test]
    #[should_panic(expected = "KZG proof verification error: Invalid proofs length")]
    fn missing_proof_is_rejected() {
        let mut data = blob_data(&[blob_fixture(1), blob_fixture(2)]);
        data.proofs.pop();
        let _ = BlobStore::from(data);
    }

    #[test]
    #[should_panic(expected = "KZG proof verification error: Invalid proofs length")]
    fn proof_without_blob_is_rejected() {
        let fixture = blob_fixture(1);
        let _ = BlobStore::from(BlobData {
            blobs: vec![],
            commitments: vec![],
            proofs: vec![fixture.proof],
        });
    }

    #[test]
    #[should_panic(expected = "KZG proof verification error: Invalid commitments length")]
    fn commitment_without_blob_is_rejected() {
        let fixture = blob_fixture(1);
        let _ = BlobStore::from(BlobData {
            blobs: vec![],
            commitments: vec![fixture.commitment],
            proofs: vec![fixture.proof],
        });
    }
}
