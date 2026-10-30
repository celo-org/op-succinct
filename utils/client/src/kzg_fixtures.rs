//! Real KZG vectors for tests, generated with c-kzg and the Ethereum mainnet trusted setup.

use c_kzg::{ethereum_kzg_settings, BYTES_PER_BLOB};

/// A blob, its commitment and its blob KZG proof.
pub struct BlobFixture {
    pub blob: kzg_rs::Blob,
    pub commitment: kzg_rs::Bytes48,
    pub proof: kzg_rs::Bytes48,
}

/// Builds a deterministic, valid blob from `seed`. Every field element keeps its top byte zero so
/// it stays below the BLS12-381 scalar modulus. The seed scales each element differently, so blobs
/// from different seeds never differ by a constant (which would give them identical proofs).
fn c_kzg_blob(seed: u8) -> c_kzg::Blob {
    let mut bytes = [0u8; BYTES_PER_BLOB];
    for (i, element) in bytes.chunks_exact_mut(32).enumerate() {
        let value = ((i as u64 + 1) * (seed as u64 + 1)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        element[1..9].copy_from_slice(&value.to_be_bytes());
    }
    c_kzg::Blob::new(bytes)
}

pub fn blob_fixture(seed: u8) -> BlobFixture {
    let settings = ethereum_kzg_settings(0);
    let blob = c_kzg_blob(seed);
    let commitment = settings.blob_to_kzg_commitment(&blob).unwrap().to_bytes();
    let proof = settings.compute_blob_kzg_proof(&blob, &commitment).unwrap();
    BlobFixture {
        blob: kzg_rs::Blob::from_slice(&*blob).unwrap(),
        commitment: kzg_rs::Bytes48(*commitment),
        proof: kzg_rs::Bytes48(*proof.to_bytes()),
    }
}
