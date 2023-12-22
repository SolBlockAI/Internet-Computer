use ic_crypto_internal_threshold_sig_ecdsa::*;
use ic_crypto_test_utils_reproducible_rng::reproducible_rng;
use ic_types::*;
use rand::Rng;
use std::collections::BTreeMap;

mod test_utils;

use crate::test_utils::*;

fn insufficient_dealings(r: Result<ProtocolRound, ThresholdEcdsaError>) {
    match r {
        Err(ThresholdEcdsaError::InsufficientDealings) => {}
        Err(e) => panic!("Unexpected error {:?}", e),
        Ok(r) => panic!("Unexpected success {:?}", r),
    }
}

#[test]
fn should_reshare_transcripts_correctly() -> Result<(), ThresholdEcdsaError> {
    let mut rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(&mut rng);
        let setup = ProtocolSetup::new(cfg, 4, 2, random_seed)?;

        let no_corruption = 0; // number of corrupted dealings == 0
        let corrupted_dealings = 1;

        // First create a transcript of random dealings
        let random = ProtocolRound::random(&setup, 4, corrupted_dealings)?;

        // Now reshare the random value twice

        // 1 dealing is not sufficient
        insufficient_dealings(ProtocolRound::reshare_of_masked(
            &setup,
            &random,
            1,
            no_corruption,
        ));

        // 2, 3, or 4 works:
        let reshared2 = ProtocolRound::reshare_of_masked(&setup, &random, 2, corrupted_dealings)?;
        let reshared3 = ProtocolRound::reshare_of_masked(&setup, &random, 3, corrupted_dealings)?;
        let reshared4 = ProtocolRound::reshare_of_masked(&setup, &random, 4, corrupted_dealings)?;

        // The same value is committed in the resharings despite different dealing cnt
        assert_eq!(reshared2.constant_term(), reshared3.constant_term());
        assert_eq!(reshared2.constant_term(), reshared4.constant_term());

        // Now reshare the now-unmasked value
        insufficient_dealings(ProtocolRound::reshare_of_unmasked(
            &setup,
            &reshared2,
            1,
            no_corruption,
        ));
        let unmasked =
            ProtocolRound::reshare_of_unmasked(&setup, &reshared2, 2, corrupted_dealings)?;
        assert_eq!(reshared2.constant_term(), unmasked.constant_term());

        // Now multiply the masked and umasked values
        // We need 3 dealings to multiply
        insufficient_dealings(ProtocolRound::multiply(
            &setup,
            &random,
            &unmasked,
            1,
            no_corruption,
        ));
        insufficient_dealings(ProtocolRound::multiply(
            &setup,
            &random,
            &unmasked,
            2,
            no_corruption,
        ));
        let _product = ProtocolRound::multiply(&setup, &random, &unmasked, 3, corrupted_dealings)?;
    }

    Ok(())
}

#[test]
fn should_multiply_transcripts_correctly() -> Result<(), ThresholdEcdsaError> {
    let mut rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(&mut rng);
        let setup = ProtocolSetup::new(cfg, 4, 2, random_seed)?;

        let dealers = 4;
        let corrupted_dealings = 1;

        // First create two random transcripts
        let random_a = ProtocolRound::random(&setup, dealers, corrupted_dealings)?;
        let random_b = ProtocolRound::random(&setup, dealers, corrupted_dealings)?;

        // Now reshare them both
        let random_c =
            ProtocolRound::reshare_of_masked(&setup, &random_a, dealers, corrupted_dealings)?;
        let random_d =
            ProtocolRound::reshare_of_masked(&setup, &random_b, dealers, corrupted_dealings)?;

        // Now multiply A*D and B*C (which will be the same numbers)
        let product_ad =
            ProtocolRound::multiply(&setup, &random_a, &random_d, dealers, corrupted_dealings)?;
        let product_bc =
            ProtocolRound::multiply(&setup, &random_b, &random_c, dealers, corrupted_dealings)?;

        // Now reshare AD and BC
        let reshare_ad =
            ProtocolRound::reshare_of_masked(&setup, &product_ad, dealers, corrupted_dealings)?;
        let reshare_bc =
            ProtocolRound::reshare_of_masked(&setup, &product_bc, dealers, corrupted_dealings)?;

        // The committed values of AD and BC should be the same:
        assert_eq!(reshare_ad.constant_term(), reshare_bc.constant_term());
    }

    Ok(())
}

#[test]
fn should_reshare_transcripts_with_dynamic_threshold() -> Result<(), ThresholdEcdsaError> {
    let mut rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(&mut rng);
        let mut setup = ProtocolSetup::new(cfg, 5, 2, random_seed)?;

        let no_corruption = 0; // number of corrupted dealings == 0
        let corrupted_dealings = 1;

        let random_a = ProtocolRound::random(&setup, 5, corrupted_dealings)?;

        insufficient_dealings(ProtocolRound::reshare_of_masked(
            &setup,
            &random_a,
            1,
            no_corruption,
        ));
        let reshared_b =
            ProtocolRound::reshare_of_masked(&setup, &random_a, 2, corrupted_dealings)?;

        setup.modify_threshold(1);
        setup.remove_nodes(2);
        insufficient_dealings(ProtocolRound::reshare_of_unmasked(
            &setup,
            &reshared_b,
            1,
            no_corruption,
        ));

        let reshared_c =
            ProtocolRound::reshare_of_unmasked(&setup, &reshared_b, 2, corrupted_dealings)?;
        let reshared_d =
            ProtocolRound::reshare_of_unmasked(&setup, &reshared_b, 3, corrupted_dealings)?;

        // b, c, and d all have the same value
        assert_eq!(reshared_b.constant_term(), reshared_c.constant_term());
        assert_eq!(reshared_b.constant_term(), reshared_d.constant_term());
    }

    Ok(())
}

#[test]
fn should_multiply_transcripts_with_dynamic_threshold() -> Result<(), ThresholdEcdsaError> {
    let mut rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(&mut rng);
        let mut setup = ProtocolSetup::new(cfg, 5, 2, random_seed)?;

        let corrupted_dealings = 1;

        let random_a = ProtocolRound::random(&setup, 5, corrupted_dealings)?;
        let random_b = ProtocolRound::random(&setup, 5, corrupted_dealings)?;

        let reshared_c =
            ProtocolRound::reshare_of_masked(&setup, &random_a, 3, corrupted_dealings)?;

        setup.modify_threshold(1);
        setup.remove_nodes(2);
        insufficient_dealings(ProtocolRound::multiply(
            &setup,
            &random_b,
            &reshared_c,
            1,
            0,
        ));
        insufficient_dealings(ProtocolRound::multiply(
            &setup,
            &random_b,
            &reshared_c,
            2,
            0,
        ));

        let _product =
            ProtocolRound::multiply(&setup, &random_b, &reshared_c, 3, corrupted_dealings)?;
    }

    Ok(())
}

fn random_subset<R: rand::Rng>(
    shares: &BTreeMap<NodeIndex, ThresholdEcdsaSigShareInternal>,
    include: usize,
    rng: &mut R,
) -> BTreeMap<NodeIndex, ThresholdEcdsaSigShareInternal> {
    assert!(include <= shares.len());

    let mut result = BTreeMap::new();

    let keys = shares.keys().collect::<Vec<_>>();

    while result.len() != include {
        let key_to_add = keys[rng.gen::<usize>() % keys.len()];

        if !result.contains_key(key_to_add) {
            result.insert(*key_to_add, shares[key_to_add].clone());
        }
    }

    result
}

#[test]
fn should_basic_signing_protocol_work() -> Result<(), ThresholdEcdsaError> {
    fn test_sig_serialization(
        alg: ic_types::crypto::AlgorithmId,
        sig: &ThresholdEcdsaCombinedSigInternal,
    ) -> Result<(), ThresholdEcdsaError> {
        let bytes = sig.serialize();
        let sig2 = ThresholdEcdsaCombinedSigInternal::deserialize(alg, &bytes)
            .expect("Deserialization failed");
        assert_eq!(*sig, sig2);
        Ok(())
    }

    let nodes = 10;
    let threshold = nodes / 3;
    let number_of_dealings_corrupted = threshold;

    let rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(rng);

        let setup = SignatureProtocolSetup::new(
            cfg,
            nodes,
            threshold,
            number_of_dealings_corrupted,
            random_seed,
        )?;

        let alg = setup.alg();

        let signed_message = rng.gen::<[u8; 32]>().to_vec();
        let random_beacon = Randomness::from(rng.gen::<[u8; 32]>());

        let derivation_path = DerivationPath::new_bip32(&[1, 2, 3]);
        let proto = SignatureProtocolExecution::new(
            setup.clone(),
            signed_message.clone(),
            random_beacon,
            derivation_path.clone(),
        );

        let shares = proto.generate_shares()?;

        for i in 0..=nodes {
            let shares = random_subset(&shares, i, rng);

            if shares.len() < threshold {
                assert!(proto.generate_signature(&shares).is_err());
            } else {
                let sig = proto.generate_signature(&shares).unwrap();
                test_sig_serialization(alg, &sig)?;
                assert!(proto.verify_signature(&sig).is_ok());
            }
        }

        // Test that another run of the protocol generates signatures
        // which are not verifiable in the earlier one (due to different rho)
        let random_beacon2 = Randomness::from(rng.gen::<[u8; 32]>());
        let proto2 =
            SignatureProtocolExecution::new(setup, signed_message, random_beacon2, derivation_path);

        let shares = proto2.generate_shares()?;
        let sig = proto2.generate_signature(&shares).unwrap();
        test_sig_serialization(alg, &sig)?;

        assert!(proto.verify_signature(&sig).is_err());
        assert!(proto2.verify_signature(&sig).is_ok());
    }

    Ok(())
}

#[test]
fn invalid_signatures_are_rejected() -> Result<(), ThresholdEcdsaError> {
    let nodes = 13;
    let threshold = (nodes + 2) / 3;
    let number_of_dealings_corrupted = 0;

    let rng = &mut reproducible_rng();

    for cfg in TestConfig::all() {
        let random_seed = Seed::from_rng(rng);

        let setup = SignatureProtocolSetup::new(
            cfg,
            nodes,
            threshold,
            number_of_dealings_corrupted,
            random_seed,
        )?;

        let alg = setup.alg();

        let signed_message = rng.gen::<[u8; 32]>().to_vec();
        let random_beacon = Randomness::from(rng.gen::<[u8; 32]>());

        let derivation_path = DerivationPath::new_bip32(&[1, 2, 3]);
        let proto =
            SignatureProtocolExecution::new(setup, signed_message, random_beacon, derivation_path);

        let shares = proto.generate_shares()?;

        let sig = proto.generate_signature(&shares).unwrap();

        assert_eq!(proto.verify_signature(&sig), Ok(()));

        let sig = sig.serialize();

        assert_eq!(sig.len() % 2, 0);

        let half_sig = sig.len() / 2;

        let sig_with_r_eq_zero = {
            let mut sig_with_r_eq_zero = sig.clone();
            sig_with_r_eq_zero[..half_sig].fill(0);
            ThresholdEcdsaCombinedSigInternal::deserialize(alg, &sig_with_r_eq_zero).unwrap()
        };

        assert!(proto.verify_signature(&sig_with_r_eq_zero).is_err());

        let sig_with_s_eq_zero = {
            let mut sig_with_s_eq_zero = sig.clone();
            sig_with_s_eq_zero[half_sig..].fill(0);
            ThresholdEcdsaCombinedSigInternal::deserialize(alg, &sig_with_s_eq_zero).unwrap()
        };

        assert!(proto.verify_signature(&sig_with_s_eq_zero).is_err());

        let sig_with_high_s = {
            let s = EccScalar::deserialize(cfg.signature_curve(), &sig[half_sig..])
                .unwrap()
                .negate();

            let mut sig_with_high_s = sig;
            sig_with_high_s[half_sig..].copy_from_slice(&s.serialize());
            ThresholdEcdsaCombinedSigInternal::deserialize(alg, &sig_with_high_s).unwrap()
        };

        assert!(proto.verify_signature(&sig_with_high_s).is_err());
    }

    Ok(())
}
