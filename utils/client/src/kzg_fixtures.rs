//! Real KZG vectors for tests, generated with c-kzg and the Ethereum mainnet trusted setup.

use alloy_primitives::hex_literal::hex;
use c_kzg::{ethereum_kzg_settings, BYTES_PER_BLOB};

/// A compressed G1 encoding of a point on the curve but outside the prime-order subgroup
/// (x^3 + 4 is a square mod p).
pub const G1_NOT_IN_SUBGROUP: [u8; 48] = hex!(
    "8123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
);

/// A compressed G1 encoding with x = 1, which is off the curve: x^3 + 4 = 5 is not a square mod p.
pub const G1_OFF_CURVE: [u8; 48] = hex!(
    "800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"
);

/// The BLS12-381 scalar field modulus, one past the largest field element (big-endian).
pub const SCALAR_MODULUS: [u8; 32] =
    hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001");

/// A blob, its commitment and its blob KZG proof.
pub struct BlobFixture {
    pub blob: kzg_rs::Blob,
    pub commitment: kzg_rs::Bytes48,
    pub proof: kzg_rs::Bytes48,
}

/// A KZG opening of a blob polynomial at `z`, as the point evaluation precompile receives it.
pub struct OpeningFixture {
    pub commitment: [u8; 48],
    pub z: [u8; 32],
    pub y: [u8; 32],
    pub proof: [u8; 48],
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

pub fn opening_fixture(seed: u8, z: [u8; 32]) -> OpeningFixture {
    let settings = ethereum_kzg_settings(0);
    let blob = c_kzg_blob(seed);
    let commitment = settings.blob_to_kzg_commitment(&blob).unwrap().to_bytes();
    let (proof, y) = settings.compute_kzg_proof(&blob, &z.into()).unwrap();
    OpeningFixture { commitment: *commitment, z, y: *y, proof: *proof.to_bytes() }
}
