use super::*;

use crate::pb::v1::Vote;
use ic_nns_common::pb::v1::ProposalId;
use lazy_static::lazy_static;
use pretty_assertions::assert_eq;

// TODO(NNS1-2497): Add tests that fail if our BoundedStorage types grow. This
// way, people are very aware of how they might be eating into our headroom.

lazy_static! {
    static ref MODEL_NEURON: Neuron = Neuron {
        id: Some(NeuronId { id: 42 }),
        cached_neuron_stake_e8s: 0xCAFE, // Yummy.

        hot_keys: vec![
            PrincipalId::new_user_test_id(100),
            PrincipalId::new_user_test_id(101),
        ],

        followees: hashmap! {
            0 => Followees {
                followees: vec![
                    NeuronId { id: 200 },
                    NeuronId { id: 201 },
                ],
            },
            1 => Followees {
                followees: vec![
                    NeuronId { id: 210 },
                    NeuronId { id: 211 },
                ],
            },
        },

        recent_ballots: vec![
            BallotInfo {
                proposal_id: Some(ProposalId { id: 300 }),
                vote: Vote::Yes as i32,
            },
            BallotInfo {
                proposal_id: Some(ProposalId { id: 301 }),
                vote: Vote::No as i32,
            },
        ],

        known_neuron_data: Some(KnownNeuronData {
            name: "Fabulous".to_string(),
            description: Some("Follow MeEe for max rewards!".to_string()),
        }),

        transfer: Some(NeuronStakeTransfer {
            transfer_timestamp: 123_456_789,
            from: Some(PrincipalId::new_user_test_id(400)),
            from_subaccount: vec![4, 0x01],
            to_subaccount: vec![4, 0x02],
            neuron_stake_e8s: 403,
            block_height: 404,
            memo: 405,
        }),

        ..Default::default()
    };
}

/// Summary:
///
///   1. create
///   2. bad create
///   3. read to verify create
///   4. bad read
///
///   5. update
///   6. read to verify the update
///   7. bad update
///   8. read to verify bad update
///   9. update: This time, with None singletons.
///   10. read to verify
///
///   11. delete
///   12. bad delete: repeat
///   13. read to verify.
#[test]
fn test_store_simplest_nontrivial_case() {
    let mut store = new_heap_based();

    // 1. Create a Neuron.
    let neuron_1 = MODEL_NEURON.clone();
    assert_eq!(store.create(neuron_1.clone()), Ok(()));

    // 2. Bad create: use an existing NeuronId. This should result in an
    // InvalidCommand Err.
    let bad_create_result = store.create(Neuron {
        id: Some(NeuronId { id: 42 }),
        cached_neuron_stake_e8s: 0xDEAD_BEEF,
        ..Default::default()
    });
    match &bad_create_result {
        Err(err) => {
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            assert_eq!(
                ErrorType::from_i32(*error_type),
                Some(ErrorType::PreconditionFailed),
                "{:?}",
                err,
            );

            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("already in use"), "{:?}", err);
            assert!(error_message.contains("42"), "{:?}", err);
        }

        _ => panic!(
            "create(evil_twin_neuron) did not result in an Err: {:?}",
            bad_create_result
        ),
    }

    // 3. Read back the first neuron (the second one should have no effect).
    assert_eq!(store.read(NeuronId { id: 42 }), Ok(neuron_1.clone()),);

    // 4. Bad read: Unknown NeuronId. This should result in a NotFound Err.
    let bad_read_result = store.read(NeuronId { id: 0xDEAD_BEEF });
    match &bad_read_result {
        Err(err) => {
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            assert_eq!(
                ErrorType::from_i32(*error_type),
                Some(ErrorType::NotFound),
                "{:?}",
                err,
            );

            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("unable to find"), "{:?}", err);
            assert!(error_message.contains("3735928559"), "{:?}", err); // 0xDEAD_BEEF
        }

        _ => panic!(
            "read(0xDEAD) did not result in an Err: {:?}",
            bad_read_result
        ),
    }

    // 5. Update existing neuron.

    // Derive neuron_5 from neuron_1 by adding entries to collections (to make
    // sure the updating collections works).
    let neuron_5 = {
        let mut hot_keys = neuron_1.hot_keys;
        hot_keys.push(PrincipalId::new_user_test_id(102));

        let mut followees = neuron_1.followees;
        assert_eq!(
            followees.insert(
                7,
                Followees {
                    followees: vec![NeuronId { id: 220 }]
                }
            ),
            None,
        );

        let mut recent_ballots = neuron_1.recent_ballots;
        recent_ballots.push(BallotInfo {
            proposal_id: Some(ProposalId { id: 303 }),
            vote: Vote::Yes as i32,
        });

        let mut known_neuron_data = neuron_1.known_neuron_data;
        known_neuron_data.as_mut().unwrap().name = "I changed my mind".to_string();

        let mut transfer = neuron_1.transfer;
        transfer.as_mut().unwrap().memo = 405_405;

        Neuron {
            cached_neuron_stake_e8s: 0xFEED, // After drink, we eat.

            hot_keys,
            followees,
            recent_ballots,

            known_neuron_data,
            transfer,

            ..neuron_1
        }
    };
    assert_eq!(store.update(neuron_5.clone()), Ok(()));

    // 6. Read to verify update.
    assert_eq!(store.read(NeuronId { id: 42 }), Ok(neuron_5.clone()));

    // 7. Bad update: Neuron not found (unknown ID).
    let update_result = store.update(Neuron {
        id: Some(NeuronId { id: 0xDEAD_BEEF }),
        cached_neuron_stake_e8s: 0xBAD_F00D,
        ..Default::default()
    });
    match &update_result {
        // This is what we expected.
        Err(err) => {
            // Take a closer look at err.
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            // Inspect type.
            let error_type = ErrorType::from_i32(*error_type);
            assert_eq!(error_type, Some(ErrorType::NotFound), "{:?}", err);

            // Next, turn to error_message.
            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("update"), "{:?}", err);
            assert!(error_message.contains("existing"), "{:?}", err);
            assert!(error_message.contains("neuron"), "{:?}", err);
            assert!(error_message.contains("there was none"), "{:?}", err);

            assert!(error_message.contains("id"), "{:?}", err);
            assert!(error_message.contains("3735928559"), "{:?}", err); // 0xDEAD_BEEF

            assert!(
                error_message.contains("cached_neuron_stake_e8s"),
                "{:?}",
                err,
            );
            assert!(error_message.contains("195948557"), "{:?}", err); // 0xBAD_F00D
        }

        // Any other result is bad.
        _ => panic!("{:#?}", update_result),
    }

    // 8. Read to verify bad update.
    let read_result = store.read(NeuronId { id: 0xDEAD_BEEF });
    match &read_result {
        // This is what we expected.
        Err(err) => {
            // Take a closer look at err.
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            // Inspect type.
            let error_type = ErrorType::from_i32(*error_type);
            assert_eq!(error_type, Some(ErrorType::NotFound), "{:?}", err);

            // Next, turn to error_message.
            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("unable to find"), "{:?}", err);
            assert!(error_message.contains("3735928559"), "{:?}", err); // 0xDEAD_BEEF
        }

        _ => panic!("read did not return Err: {:?}", read_result),
    }

    // 9. Update again.
    let neuron_9 = Neuron {
        known_neuron_data: None,
        transfer: None,
        ..neuron_5
    };
    assert_eq!(store.update(neuron_9.clone()), Ok(()));

    // 10. Read to verify second update.
    assert_eq!(store.read(NeuronId { id: 42 }), Ok(neuron_9));

    // 11. Delete.
    assert_eq!(store.delete(NeuronId { id: 42 }), Ok(()));

    // 12. Bad delete: repeat.
    let delete_result = store.delete(NeuronId { id: 42 });
    match &delete_result {
        // This is what we expected.
        Err(err) => {
            // Take a closer look at err.
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            // Inspect type.
            let error_type = ErrorType::from_i32(*error_type);
            assert_eq!(error_type, Some(ErrorType::NotFound), "{:?}", err);

            // Next, turn to error_message.
            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("not found"), "{:?}", err);
            assert!(error_message.contains("42"), "{:?}", err);
        }

        _ => panic!("second delete did not return Err: {:?}", delete_result),
    }

    // 13. Read to verify delete.
    let read_result = store.read(NeuronId { id: 42 });
    match &read_result {
        // This is what we expected.
        Err(err) => {
            // Take a closer look at err.
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            // Inspect type.
            let error_type = ErrorType::from_i32(*error_type);
            assert_eq!(error_type, Some(ErrorType::NotFound), "{:?}", err);

            // Next, turn to error_message.
            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("unable to find"), "{:?}", err);
            assert!(error_message.contains("42"), "{:?}", err);
        }

        _ => panic!("read did not return Err: {:?}", read_result),
    }

    // Make sure delete is actually thorough. I.e. no dangling references.
    // Here, we access privates. Elsewhere, we do not do this. I suppose
    // StableNeuronStore could have a pub is_internally_consistent method.
    assert!(store.hot_keys_map.is_empty());
    assert!(store.followees_map.is_empty());
    assert!(store.recent_ballots_map.is_empty());
    assert!(store.known_neuron_data_map.is_empty());
    assert!(store.transfer_map.is_empty());
}

/// Summary:
///
///   1. upsert (effectively, an insert)
///   2. read to verify
///   3. upsert same ID (effectively, an update)
///   4. read to verify
#[test]
fn test_store_upsert() {
    let mut store = new_heap_based();

    let neuron = MODEL_NEURON.clone();
    let neuron_id = neuron.id.unwrap();

    // 1. upsert (entry not already present)
    assert_eq!(store.upsert(neuron.clone()), Ok(()));

    // 2. read to verify
    assert_eq!(store.read(neuron_id), Ok(neuron.clone()));

    // Modify neuron.
    let updated_neuron = {
        let mut hot_keys = neuron.hot_keys;
        hot_keys.push(PrincipalId::new_user_test_id(999_000));

        let mut followees = neuron.followees;
        followees
            .entry(0)
            .or_default()
            .followees
            .push(NeuronId { id: 999_001 });

        let mut recent_ballots = neuron.recent_ballots;
        recent_ballots.insert(
            0,
            BallotInfo {
                proposal_id: Some(ProposalId { id: 999_002 }),
                vote: Vote::No as i32,
            },
        );

        let mut known_neuron_data = neuron.known_neuron_data;
        known_neuron_data.as_mut().unwrap().description = None;

        let mut transfer = neuron.transfer;
        let transfer = None;

        Neuron {
            cached_neuron_stake_e8s: 0xCAFE,

            hot_keys,
            followees,
            recent_ballots,

            known_neuron_data,
            transfer,

            ..neuron
        }
    };

    // 3. upsert (change an existing entry)
    assert_eq!(store.upsert(updated_neuron.clone()), Ok(()));

    // 4. read to verify
    assert_eq!(store.read(neuron_id), Ok(updated_neuron));
}
