#![allow(dead_code)] // TODO(NNS1-2409): remove when it is used by NNS Governance.

use crate::pb::v1::{governance_error::ErrorType, GovernanceError};

use ic_nns_common::pb::v1::NeuronId;
use ic_stable_structures::{Memory, StableBTreeMap};
use icp_ledger::Subaccount;

/// An index to make it easy to lookup neuron id by subaccount.
pub struct NeuronSubaccountIndex<M: Memory> {
    subaccount_to_id: StableBTreeMap<[u8; 32], u64, M>,
}

impl<M: Memory> NeuronSubaccountIndex<M> {
    pub fn new(memory: M) -> Self {
        Self {
            subaccount_to_id: StableBTreeMap::init(memory),
        }
    }

    /// Adds a neuron into the index. Returns error if the subaccount already exists
    /// in the index and the index should remain unchanged.
    pub fn add_neuron_subaccount(
        &mut self,
        neuron_id: NeuronId,
        subaccount: &Subaccount,
    ) -> Result<(), GovernanceError> {
        let previous_neuron_id = self.subaccount_to_id.insert(subaccount.0, neuron_id.id);
        match previous_neuron_id {
            None => Ok(()),
            Some(previous_neuron_id) => {
                self.subaccount_to_id
                    .insert(subaccount.0, previous_neuron_id);
                Err(GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    format!("Subaccount {:?} already exists in the index", subaccount.0),
                ))
            }
        }
    }

    /// Removes a neuron from the index. Returns error if the neuron_id removed from the index is
    /// unexpected, and the index should remain unchanged if that happens.
    pub fn remove_neuron_subaccount(
        &mut self,
        neuron_id: NeuronId,
        subaccount: &Subaccount,
    ) -> Result<(), GovernanceError> {
        let previous_neuron_id = self.subaccount_to_id.remove(&subaccount.0);

        match previous_neuron_id {
            Some(previous_neuron_id) => {
                if previous_neuron_id == neuron_id.id {
                    Ok(())
                } else {
                    self.subaccount_to_id
                        .insert(subaccount.0, previous_neuron_id);
                    Err(GovernanceError::new_with_message(
                        ErrorType::PreconditionFailed,
                        format!(
                            "Subaccount {:?} exists in the index with a different neuron id {}",
                            subaccount.0, previous_neuron_id
                        ),
                    ))
                }
            }
            None => Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!("Subaccount {:?} already absent in the index", subaccount.0),
            )),
        }
    }

    /// Finds the neuron id by subaccount if it exists.
    pub fn get_neuron_id_by_subaccount(&self, subaccount: &Subaccount) -> Option<NeuronId> {
        self.subaccount_to_id
            .get(&subaccount.0)
            .map(|id| NeuronId { id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn add_single_neuron() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        assert!(index
            .add_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());

        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([1u8; 32])),
            Some(NeuronId { id: 1 })
        );
    }

    #[test]
    fn add_and_remove_neuron() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        assert!(index
            .add_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());
        assert!(index
            .remove_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());

        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([1u8; 32])),
            None
        );
    }

    #[test]
    fn add_neuron_with_same_subaccount_fails() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        assert!(index
            .add_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());
        assert_matches!(
            index.add_neuron_subaccount(NeuronId { id: 2 }, &Subaccount([1u8; 32])),
            Err(GovernanceError{error_type, error_message: message})
                if error_type == ErrorType::PreconditionFailed as i32 && message.contains("already exists in the index")
        );

        // The index should still have the first neuron.
        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([1u8; 32])),
            Some(NeuronId { id: 1 })
        );
    }

    #[test]
    fn remove_neuron_already_absent_fails() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        // The index is empty so remove should fail.
        assert_matches!(
            index.remove_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32])),
            Err(GovernanceError{error_type, error_message: message})
                if error_type == ErrorType::PreconditionFailed as i32 && message.contains("already absent in the index")
        );
    }

    #[test]
    fn remove_neuron_with_wrong_neuron_id_fails() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        assert!(index
            .add_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());
        assert_matches!(
            index.remove_neuron_subaccount(NeuronId { id: 2 }, &Subaccount([1u8; 32])),
            Err(GovernanceError{error_type, error_message: message})
                if error_type == ErrorType::PreconditionFailed as i32 && message.contains("exists in the index with a different neuron id")
        );

        // The index should still have the first neuron.
        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([1u8; 32])),
            Some(NeuronId { id: 1 })
        );
    }

    #[test]
    fn add_multiple_neurons() {
        let mut index = NeuronSubaccountIndex::new(VectorMemory::default());

        assert!(index
            .add_neuron_subaccount(NeuronId { id: 1 }, &Subaccount([1u8; 32]))
            .is_ok());
        assert!(index
            .add_neuron_subaccount(NeuronId { id: 2 }, &Subaccount([2u8; 32]))
            .is_ok());

        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([1u8; 32])),
            Some(NeuronId { id: 1 })
        );
        assert_eq!(
            index.get_neuron_id_by_subaccount(&Subaccount([2u8; 32])),
            Some(NeuronId { id: 2 })
        );
    }
}
