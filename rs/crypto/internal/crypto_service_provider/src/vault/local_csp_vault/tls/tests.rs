#![allow(clippy::unwrap_used)]

use crate::vault::local_csp_vault::tls::SecretKeyStoreInsertionError;
use crate::vault::test_utils::sks::secret_key_store_containing_key_with_invalid_encoding;
use crate::vault::test_utils::sks::secret_key_store_with_duplicated_key_id_error_on_insert;
use crate::LocalCspVault;
use assert_matches::assert_matches;
use ic_test_utilities::FastForwardTimeSource;
use ic_types_test_utils::ids::node_test_id;

const NODE_1: u64 = 4241;

mod keygen {
    use super::*;
    use crate::key_id::KeyId;
    use crate::public_key_store::mock_pubkey_store::MockPublicKeyStore;
    use crate::public_key_store::PublicKeySetOnceError;
    use crate::secret_key_store::mock_secret_key_store::MockSecretKeyStore;
    use crate::vault::api::CspTlsKeygenError;
    use crate::vault::api::PublicKeyStoreCspVault;
    use crate::vault::api::SecretKeyStoreCspVault;
    use crate::vault::api::TlsHandshakeCspVault;
    use crate::vault::local_csp_vault::LocalCspVault;
    use ic_crypto_tls_interfaces::TlsPublicKeyCert;
    use ic_test_utilities::MockTimeSource;
    use ic_types::time::Time;
    use mockall::Sequence;
    use openssl::asn1::Asn1Time;
    use openssl::asn1::Asn1TimeRef;
    use openssl::bn::BigNum;
    use openssl::nid::Nid;
    use openssl::x509::{X509NameEntries, X509VerifyResult, X509};
    use proptest::proptest;
    use rand::SeedableRng;
    use rand::{CryptoRng, Rng};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    const NOT_AFTER: &str = "99991231235959Z";
    const NANOS_PER_SEC: i64 = 1_000_000_000;

    #[test]
    fn should_generate_tls_key_pair_and_store_certificate() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");
        let key_id = KeyId::try_from(&cert).unwrap();

        assert!(csp_vault.sks_contains(&key_id).expect("SKS call failed"));
        assert_eq!(
            csp_vault
                .current_node_public_keys()
                .expect("missing public keys")
                .tls_certificate
                .expect("missing tls certificate"),
            cert.to_proto()
        );
    }

    #[test]
    fn should_fail_if_secret_key_insertion_yields_duplicate_error() {
        let duplicated_key_id = KeyId::from([42; 32]);
        let secret_key_store =
            secret_key_store_with_duplicated_key_id_error_on_insert(duplicated_key_id);
        let csp_vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(secret_key_store)
            .build();

        let result = csp_vault.gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER);

        assert_matches!(
            result,
            Err(CspTlsKeygenError::DuplicateKeyId { key_id }) if key_id ==  duplicated_key_id
        );
    }

    #[test]
    fn should_return_der_encoded_self_signed_certificate() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");

        let x509_cert = &x509(&cert);
        let public_key = x509_cert
            .public_key()
            .expect("Missing public key in a certificate.");
        assert_eq!(x509_cert.verify(&public_key).ok(), Some(true));
        assert_eq!(x509_cert.issued(x509_cert), X509VerifyResult::OK);
    }

    #[test]
    fn should_set_cert_subject_cn_as_node_id() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");

        let x509_cert = &x509(&cert);
        assert_eq!(cn_entries(x509_cert).count(), 1);
        let subject_cn = cn_entries(x509_cert)
            .next()
            .expect("Missing 'subject CN' entry in a certificate");
        let expected_subject_cn = node_test_id(NODE_1).get().to_string();
        assert_eq!(expected_subject_cn.as_bytes(), subject_cn.data().as_slice());
    }

    #[test]
    fn should_use_stable_node_id_string_representation_as_subject_cn() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");
        let cert_x509 = x509(&cert);

        let subject_cn = cn_entries(&cert_x509)
            .next()
            .expect("Missing 'subject CN' entry in a certificate");
        assert_eq!(b"w43gn-nurca-aaaaa-aaaap-2ai", subject_cn.data().as_slice());
    }

    #[test]
    fn should_set_cert_issuer_cn_as_node_id() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");
        let cert_x509 = x509(&cert);

        let issuer_cn = cert_x509
            .issuer_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .expect("Missing 'issuer CN' entry in a certificate");
        let expected_issuer_cn = node_test_id(NODE_1).get().to_string();
        assert_eq!(expected_issuer_cn.as_bytes(), issuer_cn.data().as_slice());
    }

    #[test]
    fn should_not_set_cert_subject_alt_name() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");

        let subject_alt_names = x509(&cert).subject_alt_names();
        assert!(subject_alt_names.is_none());
    }

    #[test]
    fn should_set_random_cert_serial_number() {
        pub const FIXED_SEED: u64 = 42;
        let csp_vault = LocalCspVault::builder_for_test()
            .with_rng(csprng_seeded_with(FIXED_SEED))
            .build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");

        let cert_serial = x509(&cert)
            .serial_number()
            .to_bn()
            .expect("Failed parsing SN as BigNum.");
        let expected_randomness = csprng_seeded_with(FIXED_SEED).gen::<[u8; 19]>();
        let expected_serial =
            BigNum::from_slice(&expected_randomness).expect("Failed parsing random bits as BigNum");
        assert_eq!(expected_serial, cert_serial);
    }

    #[test]
    fn should_set_different_serial_numbers_for_multiple_certs() {
        let csp_vault_factory = &(|| LocalCspVault::builder_for_test().build());
        const SAMPLE_SIZE: usize = 20;
        let mut serial_samples = BTreeSet::new();
        for _i in 0..SAMPLE_SIZE {
            let cert = csp_vault_factory()
                .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
                .expect("Generation of TLS keys failed.");
            serial_samples.insert(serial_number(&cert));
        }
        assert_eq!(serial_samples.len(), SAMPLE_SIZE);
    }

    #[test]
    fn should_set_cert_not_before_correctly() {
        use chrono::prelude::*;
        use ic_crypto_test_utils_reproducible_rng::reproducible_rng;
        use ic_interfaces::time_source::TimeSource;
        use ic_types::time::Time;
        use std::time::{Duration, UNIX_EPOCH};

        const NANOS_PER_SEC: u64 = 1_000_000_000;
        const MAX_TIME_SECS: u64 = u64::MAX / NANOS_PER_SEC;
        const GRACE_PERIOD_SECS: u64 = 120;

        let mut rng = reproducible_rng();

        // generate random values
        let mut inputs: Vec<_> = (0..100).map(|_| rng.gen_range(0..MAX_TIME_SECS)).collect();

        // append edge cases (when time is below `GRACE_PERIOD_SECS`)
        inputs.push(0);
        inputs.push(1);
        inputs.push(2);
        inputs.push(GRACE_PERIOD_SECS - 1);
        inputs.push(GRACE_PERIOD_SECS);

        for random_current_time_secs in inputs {
            let time_source = FastForwardTimeSource::new();
            time_source
                .set_time(
                    Time::from_secs_since_unix_epoch(random_current_time_secs)
                        .expect("failed to convert time"),
                )
                .expect("failed to set time");
            let csp_vault = LocalCspVault::builder_for_test()
                .with_time_source(Arc::clone(&time_source) as _)
                .build();

            let cert = csp_vault
                .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
                .expect("error generating TLS certificate");

            // We are deliberately not using `Asn1Time::from_unix` used in
            // production to ensure the right time unit is passed.
            let expected_not_before = {
                let secs = time_source
                    .get_relative_time()
                    .as_secs_since_unix_epoch()
                    .saturating_sub(GRACE_PERIOD_SECS);
                let utc = DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(secs));
                utc.format("%b %e %H:%M:%S %Y GMT").to_string()
            };

            assert_eq!(x509(&cert).not_before().to_string(), expected_not_before);
        }
    }

    #[test]
    fn should_set_cert_not_after_correctly() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");
        assert!(
            x509(&cert).not_after()
                == Asn1Time::from_str_x509(NOT_AFTER).expect("Failed parsing string as Asn1Time")
        );
    }

    #[test]
    fn should_return_error_on_invalid_not_after_date() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let invalid_not_after = "invalid_not_after_date";
        let result = csp_vault.gen_tls_key_pair(node_test_id(NODE_1), invalid_not_after);
        assert_matches!(result, Err(CspTlsKeygenError::InvalidArguments { message })
            if message.contains("invalid X.509 certificate expiration date (notAfter=invalid_not_after_date): failed to parse ASN1 datetime format")
        );
    }

    #[test]
    fn should_return_error_if_not_after_date_is_not_after_not_before_date() {
        let csp_vault = LocalCspVault::builder_for_test()
            .with_time_source(FastForwardTimeSource::new())
            .build();
        const UNIX_EPOCH: &str = "19700101000000Z";
        const UNIX_EPOCH_AS_TIME_DATE: &str = "1970-01-01 0:00:00.0 +00:00:00";

        let result = csp_vault.gen_tls_key_pair(node_test_id(NODE_1), UNIX_EPOCH);
        let expected_message = format!("notBefore date ({UNIX_EPOCH_AS_TIME_DATE}) must be before notAfter date ({UNIX_EPOCH_AS_TIME_DATE})");
        assert_matches!(result, Err(CspTlsKeygenError::InvalidArguments { message })
            if message == expected_message
        );
    }

    #[test]
    fn should_return_error_if_not_after_date_does_not_equal_99991231235959z() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let unexpected_not_after_date = "25670102030405Z";

        let result = csp_vault.gen_tls_key_pair(node_test_id(NODE_1), unexpected_not_after_date);

        assert_matches!(result, Err(CspTlsKeygenError::InternalError {internal_error})
            if internal_error.contains("TLS certificate validation error") &&
            internal_error.contains("notAfter date is not RFC 5280 value 99991231235959Z"));
    }

    proptest! {
        #[test]
        fn should_pass_the_correct_time_and_date(secs in 0..i64::MAX / NANOS_PER_SEC) {
            const GRACE_PERIOD_SECS: i64 = 120;

            let mut mock = MockTimeSource::new();
            mock.expect_get_relative_time()
                .return_const(Time::from_secs_since_unix_epoch(secs as u64).expect("failed to create Time object"));
            let csp_vault = LocalCspVault::builder_for_test()
                .with_time_source(Arc::new(mock))
                .build();

            let cert = csp_vault
                .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
                .expect("Failed to generate certificate");
            let cert_x509 = x509(&cert);
            let not_before = cert_x509.not_before();

            let expected_not_before: &Asn1TimeRef = &Asn1Time::from_unix(secs.saturating_sub(GRACE_PERIOD_SECS)).expect("failed to convert time");
            let diff = not_before.diff(expected_not_before).expect("failed to obtain time diff");

            assert_eq!(diff, openssl::asn1::TimeDiff{
                days: 0,
                secs: 0,
            });
        }
    }

    #[test]
    fn should_store_tls_secret_key_before_certificate() {
        let mut seq = Sequence::new();
        let mut sks = MockSecretKeyStore::new();
        sks.expect_insert()
            .times(1)
            .returning(|_key, _key_id, _scope| Ok(()))
            .in_sequence(&mut seq);
        let mut pks = MockPublicKeyStore::new();
        pks.expect_set_once_tls_certificate()
            .times(1)
            .returning(|_key| Ok(()))
            .in_sequence(&mut seq);
        let vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(sks)
            .with_public_key_store(pks)
            .build();

        let _ = vault.gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER);
    }

    #[test]
    fn should_fail_with_internal_error_if_tls_certificate_already_set() {
        let mut pks_returning_already_set_error = MockPublicKeyStore::new();
        pks_returning_already_set_error
            .expect_set_once_tls_certificate()
            .returning(|_key| Err(PublicKeySetOnceError::AlreadySet));
        let vault = LocalCspVault::builder_for_test()
            .with_public_key_store(pks_returning_already_set_error)
            .build();
        for node_id in [NODE_1, NODE_1 + 1] {
            let result = vault.gen_tls_key_pair(node_test_id(node_id), NOT_AFTER);

            assert_matches!(result,
                Err(CspTlsKeygenError::InternalError { internal_error })
                if internal_error.contains("TLS certificate already set")
            );
        }
    }

    #[test]
    fn should_fail_with_internal_error_if_tls_certificate_generated_more_than_once() {
        let vault = LocalCspVault::builder_for_test().build();
        assert!(vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .is_ok());

        for node_id in [NODE_1, NODE_1 + 1, NODE_1 + 2] {
            let result = vault.gen_tls_key_pair(node_test_id(node_id), NOT_AFTER);

            assert_matches!(result,
                Err(CspTlsKeygenError::InternalError { internal_error })
                if internal_error.contains("TLS certificate already set")
            );
        }
    }

    #[test]
    fn should_fail_with_transient_internal_error_if_tls_keygen_persistence_fails() {
        let mut pks_returning_io_error = MockPublicKeyStore::new();
        let io_error = std::io::Error::new(std::io::ErrorKind::Other, "oh no!");
        pks_returning_io_error
            .expect_set_once_tls_certificate()
            .return_once(|_key| Err(PublicKeySetOnceError::Io(io_error)));
        let vault = LocalCspVault::builder_for_test()
            .with_public_key_store(pks_returning_io_error)
            .build();
        let result = vault.gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER);

        assert_matches!(result,
            Err(CspTlsKeygenError::TransientInternalError { internal_error })
            if internal_error.contains("IO error")
        );
    }

    #[test]
    fn should_fail_with_transient_internal_error_if_node_signing_secret_key_persistence_fails_due_to_io_error(
    ) {
        let mut sks_returning_io_error = MockSecretKeyStore::new();
        let expected_io_error = "cannot write to file".to_string();
        sks_returning_io_error
            .expect_insert()
            .times(1)
            .return_const(Err(SecretKeyStoreInsertionError::TransientError(
                expected_io_error.clone(),
            )));
        let vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(sks_returning_io_error)
            .build();

        let result = vault.gen_tls_key_pair(node_test_id(42), NOT_AFTER);

        assert_matches!(
            result,
            Err(CspTlsKeygenError::TransientInternalError { internal_error })
            if internal_error.contains(&expected_io_error)
        );
    }

    #[test]
    fn should_fail_with_internal_error_if_node_signing_secret_key_persistence_fails_due_to_serialization_error(
    ) {
        let mut sks_returning_serialization_error = MockSecretKeyStore::new();
        let expected_serialization_error = "cannot serialize keys".to_string();
        sks_returning_serialization_error
            .expect_insert()
            .times(1)
            .return_const(Err(SecretKeyStoreInsertionError::SerializationError(
                expected_serialization_error.clone(),
            )));
        let vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(sks_returning_serialization_error)
            .build();

        let result = vault.gen_tls_key_pair(node_test_id(42), NOT_AFTER);

        assert_matches!(
            result,
            Err(CspTlsKeygenError::InternalError { internal_error })
            if internal_error.contains(&expected_serialization_error)
        );
    }

    pub fn csprng_seeded_with(seed: u64) -> impl CryptoRng + Rng {
        rand_chacha::ChaCha20Rng::seed_from_u64(seed)
    }

    fn cn_entries(x509_cert: &X509) -> X509NameEntries {
        x509_cert.subject_name().entries_by_nid(Nid::COMMONNAME)
    }

    fn serial_number(cert: &TlsPublicKeyCert) -> BigNum {
        x509(cert)
            .serial_number()
            .to_bn()
            .expect("Failed parsing SN as BigNum")
    }

    fn x509(tls_cert: &TlsPublicKeyCert) -> X509 {
        X509::from_der(tls_cert.as_der()).expect("Error parsing DER")
    }
}

mod sign {
    use super::*;
    use crate::api::CspSigner;
    use crate::key_id::KeyId;
    use crate::vault::api::BasicSignatureCspVault;
    use crate::vault::api::CspTlsSignError;
    use crate::vault::api::SecretKeyStoreCspVault;
    use crate::vault::api::TlsHandshakeCspVault;
    use crate::vault::test_utils::ed25519_csp_pubkey_from_tls_pubkey_cert;
    use crate::Csp;
    use ic_crypto_test_utils_reproducible_rng::reproducible_rng;
    use ic_types::crypto::AlgorithmId;
    use rand::{CryptoRng, Rng, SeedableRng};
    use rand_chacha::ChaCha20Rng;

    const NOT_AFTER: &str = "99991231235959Z";
    #[test]
    fn should_sign_with_valid_key() {
        let rng = &mut reproducible_rng();
        let csp_vault = LocalCspVault::builder_for_test()
            .with_rng(ChaCha20Rng::from_seed(rng.gen()))
            .build();
        let public_key_cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");

        assert!(csp_vault
            .tls_sign(
                &random_message(rng),
                &KeyId::try_from(&public_key_cert).expect("Cannot instantiate KeyId")
            )
            .is_ok());
    }

    #[test]
    fn should_sign_verifiably() {
        let rng = &mut reproducible_rng();
        let csp_vault = LocalCspVault::builder_for_test()
            .with_rng(ChaCha20Rng::from_seed(rng.gen()))
            .build();
        let verifier = Csp::builder_for_test().build();
        let public_key_cert = csp_vault
            .gen_tls_key_pair(node_test_id(NODE_1), NOT_AFTER)
            .expect("Generation of TLS keys failed.");
        let msg = random_message(rng);

        let sig = csp_vault
            .tls_sign(
                &msg,
                &KeyId::try_from(&public_key_cert).expect("cannot instantiate KeyId"),
            )
            .expect("failed to generate signature");

        let csp_pub_key = ed25519_csp_pubkey_from_tls_pubkey_cert(&public_key_cert);
        assert!(verifier
            .verify(&sig, &msg, AlgorithmId::Ed25519, csp_pub_key)
            .is_ok());
    }

    #[test]
    fn should_fail_to_sign_if_secret_key_not_found() {
        let csp_vault = LocalCspVault::builder_for_test().build();
        let non_existent_key_id = KeyId::from(b"non-existent-key-id-000000000000".to_owned());

        let result = csp_vault.tls_sign(b"message", &non_existent_key_id);

        assert_eq!(
            result.expect_err("Unexpected success."),
            CspTlsSignError::SecretKeyNotFound {
                key_id: non_existent_key_id
            }
        );
    }

    #[test]
    fn should_fail_to_sign_if_secret_key_in_store_has_wrong_type() {
        let rng = &mut reproducible_rng();
        let csp_vault = LocalCspVault::builder_for_test()
            .with_rng(ChaCha20Rng::from_seed(rng.gen()))
            .build();
        let wrong_csp_pub_key = csp_vault
            .gen_node_signing_key_pair()
            .expect("failed to generate keys");
        let msg = random_message(rng);

        let result = csp_vault.tls_sign(&msg, &KeyId::try_from(&wrong_csp_pub_key).unwrap());

        assert_eq!(
            result.expect_err("Unexpected success."),
            CspTlsSignError::WrongSecretKeyType {
                algorithm: AlgorithmId::Tls,
                secret_key_variant: "Ed25519".to_string()
            }
        );
    }

    #[test]
    fn should_fail_to_sign_if_secret_key_in_store_has_invalid_encoding() {
        let rng = &mut reproducible_rng();
        let key_id = KeyId::from([42; 32]);
        let key_store = secret_key_store_containing_key_with_invalid_encoding(key_id);
        let csp_vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(key_store)
            .with_rng(ChaCha20Rng::from_seed(rng.gen()))
            .build();

        assert!(csp_vault.sks_contains(&key_id).expect("SKS call failed"));
        let result = csp_vault.tls_sign(&random_message(rng), &key_id);
        assert_matches!(result, Err(CspTlsSignError::MalformedSecretKey { error })
            if error.starts_with("Failed to convert TLS secret key DER from key store to Ed25519 secret key")
        );
    }

    #[test]
    fn should_fail_to_sign_if_secret_key_in_store_has_invalid_length() {
        let rng = &mut reproducible_rng();
        use crate::vault::test_utils::sks::secret_key_store_containing_key_with_invalid_length;

        let key_id = KeyId::from([43; 32]);
        let key_store = secret_key_store_containing_key_with_invalid_length(key_id);
        let csp_vault = LocalCspVault::builder_for_test()
            .with_node_secret_key_store(key_store)
            .with_rng(ChaCha20Rng::from_seed(rng.gen()))
            .build();

        let result = csp_vault.tls_sign(&random_message(rng), &key_id);
        assert_matches!(result, Err(CspTlsSignError::MalformedSecretKey { error })
            if error.starts_with("Failed to convert TLS secret key DER from key store to Ed25519 secret key")
        );
    }

    fn random_message<R: Rng + CryptoRng>(rng: &mut R) -> Vec<u8> {
        let msg_len: usize = rng.gen_range(0..1024);
        (0..msg_len).map(|_| rng.gen::<u8>()).collect()
    }
}
