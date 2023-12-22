use super::types::MetadataOptions;
use crate::common::{types::Error, utils::utils::icrc1_account_to_rosetta_accountidentifier};
use ic_base_types::PrincipalId;
use icrc_ledger_types::icrc1::account::Account;
use rosetta_core::response_types::*;
use rosetta_core::{
    convert::principal_id_from_public_key, objects::PublicKey,
    response_types::ConstructionDeriveResponse,
};

pub fn construction_derive(public_key: PublicKey) -> Result<ConstructionDeriveResponse, Error> {
    let principal_id: PrincipalId = principal_id_from_public_key(&public_key)?;
    let account: Account = principal_id.0.into();
    Ok(ConstructionDeriveResponse::new(
        None,
        Some(icrc1_account_to_rosetta_accountidentifier(&account)),
    ))
}

pub fn construction_preprocess() -> ConstructionPreprocessResponse {
    ConstructionPreprocessResponse {
        options: Some(
            MetadataOptions {
                suggested_fee: true,
            }
            .into(),
        ),
        required_public_keys: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::utils::utils::icrc1_account_to_rosetta_accountidentifier;
    use ic_canister_client_sender::{Ed25519KeyPair, Secp256k1KeyPair};
    use proptest::prelude::any;
    use proptest::proptest;
    use rosetta_core::models::RosettaSupportedKeyPair;

    fn call_construction_derive<T: RosettaSupportedKeyPair>(key_pair: &T) {
        let principal_id = key_pair.generate_principal_id().unwrap();
        let public_key = ic_rosetta_test_utils::to_public_key(key_pair);
        let account = Account {
            owner: principal_id.into(),
            subaccount: None,
        };

        let res = construction_derive(public_key);
        assert_eq!(
            res,
            Ok(ConstructionDeriveResponse {
                address: None,
                account_identifier: Some(icrc1_account_to_rosetta_accountidentifier(&account)),
                metadata: None
            })
        );
    }

    proptest! {
        #[test]
        fn test_construction_derive_ed(seed in any::<u64>()) {
            let key_pair = Ed25519KeyPair::generate_from_u64(seed);
            call_construction_derive(&key_pair);
        }

        #[test]
        fn test_construction_derive_sepc(seed in any::<u64>()) {
            let key_pair = Secp256k1KeyPair::generate_from_u64(seed);
            call_construction_derive(&key_pair);
        }
    }
}