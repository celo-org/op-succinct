//! Custom crypto provider for KZG proof verification.

use kzg_rs::{get_g2_points, Bytes32, Bytes48, KzgProof, KzgSettings};
use revm::precompile::{Crypto, PrecompileHalt};

/// Custom cryptography provider using kzg-rs for KZG proof verification.
#[derive(Debug, Default)]
pub struct CustomCrypto;

/// Trusted-setup tables for the point evaluation precompile (0x0a).
///
/// kzg-rs decodes each table into a static on first use, and in the zkVM that costs millions of
/// cycles for G1 alone. Point evaluation in kzg-rs 0.2.9 reads only `g2_points[1]`, so this loads
/// G2 on the first 0x0a call and leaves the G1 points and roots of unity empty. If a kzg-rs
/// upgrade starts indexing those tables here, the empty slice panics: the guest fails to prove,
/// but never accepts a bad proof. The tests below pin the expected outcomes with this setup.
fn point_evaluation_settings() -> KzgSettings {
    KzgSettings { roots_of_unity: &[], g1_points: &[], g2_points: get_g2_points() }
}

impl Crypto for CustomCrypto {
    fn verify_kzg_proof(
        &self,
        z: &[u8; 32],
        y: &[u8; 32],
        commitment: &[u8; 48],
        proof: &[u8; 48],
    ) -> Result<(), PrecompileHalt> {
        let z = Bytes32::from_slice(z).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let y = Bytes32::from_slice(y).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let commitment = Bytes48::from_slice(commitment)
            .map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;
        let proof =
            Bytes48::from_slice(proof).map_err(|_| PrecompileHalt::BlobVerifyKzgProofFailed)?;

        // kzg-rs returns `Ok(false)` for a well-formed but INVALID proof; that must be
        // rejected, not discarded (GHSA-pq4w-5vv8-gxhr / #924). Only `Ok(true)` passes.
        match KzgProof::verify_kzg_proof(&commitment, &z, &y, &proof, &point_evaluation_settings())
        {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(PrecompileHalt::BlobVerifyKzgProofFailed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kzg_fixtures::opening_fixture;
    use alloy_primitives::hex_literal::hex;

    /// Opening of a mainnet blob from the KZG spec tests; `z` is the largest field element.
    const COMMITMENT: [u8; 48] = hex!(
        "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca25f26936857bc3a7c2539ea8ec3a952b7"
    );
    const Z: [u8; 32] = hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000");
    const Y: [u8; 32] = hex!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9");
    const PROOF: [u8; 48] = hex!(
        "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc2160744faf0070725e00b60ad9a026a15b1a8c"
    );

    /// Evaluation points for generated openings: zero, a small value and a large one.
    const EVALUATION_POINTS: [[u8; 32]; 3] = [
        [0; 32],
        hex!("0000000000000000000000000000000000000000000000000000000000000002"),
        hex!("5eb7004fe57383e6c88b99d839937fddf3f99279353aaf8d5c9a75f91ce33c62"),
    ];

    fn verify(z: &[u8; 32], y: &[u8; 32], commitment: &[u8; 48], proof: &[u8; 48]) -> bool {
        match CustomCrypto.verify_kzg_proof(z, y, commitment, proof) {
            Ok(()) => true,
            Err(PrecompileHalt::BlobVerifyKzgProofFailed) => false,
            Err(e) => panic!("unexpected halt: {e:?}"),
        }
    }

    // The settings the precompile verifies with: G2 only.

    #[test]
    fn point_evaluation_settings_hold_only_g2() {
        let settings = point_evaluation_settings();
        assert!(settings.g1_points.is_empty());
        assert!(settings.roots_of_unity.is_empty());
        assert!(settings.g2_points.len() > 1);
    }

    // Valid openings are accepted with G2 only.

    #[test]
    fn valid_kzg_opening_is_accepted() {
        assert_eq!(CustomCrypto.verify_kzg_proof(&Z, &Y, &COMMITMENT, &PROOF), Ok(()));
    }

    #[test]
    fn generated_valid_openings_are_accepted() {
        for (seed, z) in EVALUATION_POINTS.into_iter().enumerate() {
            let o = opening_fixture(seed as u8, z);
            assert!(verify(&o.z, &o.y, &o.commitment, &o.proof), "seed {seed}");
        }
    }

    // Well-formed but wrong openings are rejected with G2 only.

    #[test]
    fn invalid_kzg_opening_is_rejected() {
        // Keep the input well-formed, but make the claimed evaluation wrong.
        let mut y = Y;
        y[31] ^= 1;

        let raw_result = KzgProof::verify_kzg_proof(
            &Bytes48::from_slice(&COMMITMENT).unwrap(),
            &Bytes32::from_slice(&Z).unwrap(),
            &Bytes32::from_slice(&y).unwrap(),
            &Bytes48::from_slice(&PROOF).unwrap(),
            &point_evaluation_settings(),
        );
        assert!(matches!(raw_result, Ok(false)), "raw kzg-rs result: {raw_result:?}");

        assert!(matches!(
            CustomCrypto.verify_kzg_proof(&Z, &y, &COMMITMENT, &PROOF),
            Err(PrecompileHalt::BlobVerifyKzgProofFailed)
        ));
    }

    #[test]
    fn generated_wrong_openings_are_rejected() {
        let o = opening_fixture(1, EVALUATION_POINTS[2]);
        let other = opening_fixture(2, EVALUATION_POINTS[2]);
        assert_ne!(o.proof, other.proof);
        assert_ne!(o.commitment, other.commitment);

        let mut wrong_y = o.y;
        wrong_y[31] ^= 1;
        assert!(!verify(&o.z, &wrong_y, &o.commitment, &o.proof), "wrong y");
        assert!(!verify(&EVALUATION_POINTS[1], &o.y, &o.commitment, &o.proof), "wrong z");
        assert!(!verify(&o.z, &o.y, &o.commitment, &other.proof), "proof of another blob");
        assert!(!verify(&o.z, &o.y, &other.commitment, &o.proof), "commitment of another blob");
    }

    // Malformed inputs fail kzg-rs's own point and scalar validation.

    #[test]
    fn malformed_inputs_are_rejected() {
        // A compressed G1 encoding whose x coordinate is not on the curve.
        let off_curve = hex!(
            "8123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        // The BLS12-381 scalar field modulus, one past the largest field element.
        let modulus = hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001");

        assert!(!verify(&Z, &Y, &off_curve, &PROOF), "commitment off curve");
        assert!(!verify(&Z, &Y, &COMMITMENT, &off_curve), "proof off curve");
        assert!(!verify(&modulus, &Y, &COMMITMENT, &PROOF), "z not canonical");
        assert!(!verify(&Z, &modulus, &COMMITMENT, &PROOF), "y not canonical");
    }
}
