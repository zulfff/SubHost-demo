// RFC 9380 appendix J.10.1 reference vectors, verified 2026-11-18.
// Source: https://github.com/cfrg/draft-irtf-cfrg-hash-to-curve/blob/main/poc/vectors/BLS12381G2_XMD:SHA-256_SSWU_RO_.json
use super::*;
use ark_bls12_381::{Fq, Fq2, G2Affine};

const DST: &[u8] = b"QUUX-V01-CS02-with-BLS12381G2_XMD:SHA-256_SSWU_RO_";

// (message, x.c0, x.c1, y.c0, y.c1); expected points are independent fixtures.
const VECTORS: &[(&[u8], &str, &str, &str, &str)] = &[
    (b"", "0x0141ebfbdca40eb85b87142e130ab689c673cf60f1a3e98d69335266f30d9b8d4ac44c1038e9dcdd5393faf5c41fb78a", "0x05cb8437535e20ecffaef7752baddf98034139c38452458baeefab379ba13dff5bf5dd71b72418717047f5b0f37da03d", "0x0503921d7f6a12805e72940b963c0cf3471c7b2a524950ca195d11062ee75ec076daf2d4bc358c4b190c0c98064fdd92", "0x12424ac32561493f3fe3c260708a12b7c620e7be00099a974e259ddc7d1f6395c3c811cdd19f1e8dbf3e9ecfdcbab8d6"),
    (b"abc", "0x02c2d18e033b960562aae3cab37a27ce00d80ccd5ba4b7fe0e7a210245129dbec7780ccc7954725f4168aff2787776e6", "0x139cddbccdc5e91b9623efd38c49f81a6f83f175e80b06fc374de9eb4b41dfe4ca3a230ed250fbe3a2acf73a41177fd8", "0x1787327b68159716a37440985269cf584bcb1e621d3a7202be6ea05c4cfe244aeb197642555a0645fb87bf7466b2ba48", "0x00aa65dae3c8d732d10ecd2c50f8a1baf3001578f71c694e03866e9f3d49ac1e1ce70dd94a733534f106d4cec0eddd16"),
    (b"abcdef0123456789", "0x121982811d2491fde9ba7ed31ef9ca474f0e1501297f68c298e9f4c0028add35aea8bb83d53c08cfc007c1e005723cd0", "0x190d119345b94fbd15497bcba94ecf7db2cbfd1e1fe7da034d26cbba169fb3968288b3fafb265f9ebd380512a71c3f2c", "0x05571a0f8d3c08d094576981f4a3b8eda0a8e771fcdcc8ecceaf1356a6acf17574518acb506e435b639353c2e14827c8", "0x0bb5e7572275c567462d91807de765611490205a941a5a6af3b1691bfe596c31225d3aabdf15faff860cb4ef17c7c3be"),
    (b"q128_qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq", "0x19a84dd7248a1066f737cc34502ee5555bd3c19f2ecdb3c7d9e24dc65d4e25e50d83f0f77105e955d78f4762d33c17da", "0x0934aba516a52d8ae479939a91998299c76d39cc0c035cd18813bec433f587e2d7a4fef038260eef0cef4d02aae3eb91", "0x14f81cd421617428bc3b9fe25afbb751d934a00493524bc4e065635b0555084dd54679df1536101b2c979c0152d09192", "0x09bcccfa036b4847c9950780733633f13619994394c23ff0b32fa6b795844f4a0673e20282d07bc69641cee04f5e5662"),
    (b"a512_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "0x01a6ba2f9a11fa5598b2d8ace0fbe0a0eacb65deceb476fbbcb64fd24557c2f4b18ecfc5663e54ae16a84f5ab7f62534", "0x11fca2ff525572795a801eed17eb12785887c7b63fb77a42be46ce4a34131d71f7a73e95fee3f812aea3de78b4d01569", "0x0b6798718c8aed24bc19cb27f866f1c9effcdbf92397ad6448b5c9db90d2b9da6cbabf48adc1adf59a1a28344e79d57e", "0x03a47f8e6d1763ba0cad63d6114c0accbef65707825a511b251a660a9b3994249ae4e63fac38b23da0c398689ee2ab52"),
];

fn field(value: &str) -> Fq {
    let bytes = hex::decode(value.trim_start_matches("0x")).unwrap();
    Fq::from_be_bytes_mod_order(&bytes)
}

#[test]
fn rfc_9380_g2_random_oracle_vectors_match() {
    for &(message, x0, x1, y0, y1) in VECTORS {
        let expected =
            G2Affine::new_unchecked(Fq2::new(field(x0), field(x1)), Fq2::new(field(y0), field(y1)));
        assert!(expected.is_on_curve());
        assert!(expected.is_in_correct_subgroup_assuming_on_curve());
        let actual = BLSScheme::hash_to_g2_in_domain(DST, message).into_affine();
        assert_eq!(actual, expected, "RFC 9380 vector for {message:?}");
    }
}

#[test]
fn protocol_and_pop_hashes_are_distinct_nonzero_subgroup_points() {
    for message in [b"".as_slice(), b"abc", b"\0", b"a longer vote payload"] {
        let signature_hash = BLSScheme::hash_to_g2_in_domain(BLS_DOMAIN, message);
        let proof_hash = BLSScheme::hash_to_g2_in_domain(BLS_POP_DOMAIN, message);
        assert_ne!(signature_hash, proof_hash);
        for point in [signature_hash, proof_hash] {
            assert!(!point.is_zero());
            let affine = point.into_affine();
            assert!(affine.is_on_curve());
            assert!(affine.is_in_correct_subgroup_assuming_on_curve());
        }
    }
}

#[test]
fn verification_rejects_off_curve_points_even_without_wire_decoding() {
    let (secret, public) = BLSScheme::keygen();
    let message = b"direct point API";
    let signature = BLSScheme::sign(&secret, message);
    let bad_public = G1Projective::new_unchecked(Fq::from(1u64), Fq::from(1u64), Fq::from(1u64));
    let bad_signature =
        G2Projective::new_unchecked(Fq2::from(1u64), Fq2::from(1u64), Fq2::from(1u64));
    assert!(!BLSScheme::verify(&bad_public, message, &signature));
    assert!(!BLSScheme::verify(&public, message, &bad_signature));
    assert!(!BLSScheme::verify_possession(&bad_public, &signature));
    assert!(!BLSScheme::verify_aggregate(&[bad_public], &[message], &signature));
}

#[test]
fn existing_canonical_secret_and_bls_public_key_encodings_stay_compatible() {
    let mut scalar_bytes = [0u8; SECRET_KEY_BYTES];
    scalar_bytes[0] = 1;
    let secret = BLSScheme::decode_secret_key(&scalar_bytes).unwrap();
    let public = BLSScheme::public_key(&secret);
    // Standard compressed BLS12-381 G1 generator, with compression/sort flags.
    let expected = hex::decode(concat!(
        "97f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586",
        "c55e83ff97a1aeffb3af00adb22c6bb"
    ))
    .unwrap();
    assert_eq!(BLSScheme::encode_public_key(&public), expected);
    assert_eq!(BLSScheme::decode_public_key(&expected), Some(public));
    assert_eq!(BLSScheme::encode_secret_key(&secret), scalar_bytes);
}
