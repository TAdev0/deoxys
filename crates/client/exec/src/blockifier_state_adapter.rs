use blockifier::execution::contract_class::ContractClass;
use blockifier::state::errors::StateError;
use blockifier::state::state_api::{StateReader, StateResult};
use mc_db::db_block_id::DbBlockId;
use mc_db::MadaraBackend;
use mp_block::BlockId;
use mp_class::to_blockifier_class;
use mp_convert::{felt_to_u64, ToFelt};
use starknet_api::core::{ChainId, ClassHash, CompiledClassHash, ContractAddress, Nonce};
use starknet_api::state::StorageKey;
use starknet_types_core::felt::Felt;
use std::sync::Arc;

/// Adapter for the db queries made by blockifier.
/// There is no actual mutable logic here - when using block production, the actual key value
/// changes in db are evaluated at the end only from the produced state diff.
pub struct BlockifierStateAdapter {
    backend: Arc<MadaraBackend>,
    /// When this value is None, we are executing the genesis block.
    pub on_top_of_block_id: Option<DbBlockId>,
    pub block_number: u64,
}

impl BlockifierStateAdapter {
    pub fn new(backend: Arc<MadaraBackend>, block_number: u64, on_top_of_block_id: Option<DbBlockId>) -> Self {
        Self { backend, on_top_of_block_id, block_number }
    }
}

impl StateReader for BlockifierStateAdapter {
    fn get_storage_at(&self, contract_address: ContractAddress, key: StorageKey) -> StateResult<Felt> {
        // The `0x1` address is reserved for block hashes: https://docs.starknet.io/architecture-and-concepts/network-architecture/starknet-state/#address_0x1
        if *contract_address.key() == Felt::ONE {
            let requested_block_number = felt_to_u64(key.0.key()).map_err(|_| StateError::OldBlockHashNotProvided)?;

            // Not found if in the last 10 blocks.
            if !block_hash_storage_check_range(
                &self.backend.chain_config().chain_id,
                self.block_number,
                requested_block_number,
            ) {
                return Ok(Felt::ZERO);
            }

            return self
                .backend
                .get_block_hash(&BlockId::Number(requested_block_number))
                .map_err(|err| {
                    log::warn!("Failed to retrieve block hash for block number {requested_block_number}: {err:#}");
                    StateError::StateReadError(format!(
                        "Failed to retrieve block hash for block number {requested_block_number}",
                    ))
                })?
                .ok_or(StateError::OldBlockHashNotProvided);
        }

        let Some(on_top_of_block_id) = self.on_top_of_block_id else { return Ok(Felt::ZERO) };

        Ok(self
            .backend
            .get_contract_storage_at(&on_top_of_block_id, &contract_address.to_felt(), &key.to_felt())
            .map_err(|err| {
                log::warn!(
                    "Failed to retrieve storage value for contract {contract_address:#?} at key {key:#?}: {err:#}"
                );
                StateError::StateReadError(format!(
                    "Failed to retrieve storage value for contract {contract_address:#?} at key {key:#?}",
                ))
            })?
            .unwrap_or(Felt::ZERO))
    }

    fn get_nonce_at(&self, contract_address: ContractAddress) -> StateResult<Nonce> {
        log::debug!("get_nonce_at for {:#?}", contract_address);
        let Some(on_top_of_block_id) = self.on_top_of_block_id else { return Ok(Nonce::default()) };

        Ok(Nonce(
            self.backend
                .get_contract_nonce_at(&on_top_of_block_id, &contract_address.to_felt())
                .map_err(|err| {
                    log::warn!("Failed to retrieve nonce for contract {contract_address:#?}: {err:#}");
                    StateError::StateReadError(format!("Failed to retrieve nonce for contract {contract_address:#?}",))
                })?
                .unwrap_or(Felt::ZERO),
        ))
    }

    fn get_class_hash_at(&self, contract_address: ContractAddress) -> StateResult<ClassHash> {
        log::debug!("get_class_hash_at for {:#?}", contract_address);
        let Some(on_top_of_block_id) = self.on_top_of_block_id else { return Ok(ClassHash::default()) };

        // Note that blockifier is fine with us returning ZERO as a class_hash if it is not found, they do the check on their end after
        Ok(ClassHash(
            self.backend
                .get_contract_class_hash_at(&on_top_of_block_id, &contract_address.to_felt())
                .map_err(|err| {
                    StateError::StateReadError(format!(
                        "Failed to retrieve class hash for contract {:#}: {:#}",
                        contract_address.0.key(),
                        err
                    ))
                })?
                .unwrap_or_default(),
        ))
    }

    fn get_compiled_contract_class(&self, class_hash: ClassHash) -> StateResult<ContractClass> {
        log::debug!("get_compiled_contract_class for {:#?}", class_hash);

        let Some(on_top_of_block_id) = self.on_top_of_block_id else {
            return Err(StateError::UndeclaredClassHash(class_hash));
        };

        let Some((_class_info, compiled_class)) =
            self.backend.get_class(&on_top_of_block_id, &class_hash.to_felt()).map_err(|err| {
                log::warn!("Failed to retrieve compiled class {class_hash:#}: {err:#}");
                StateError::StateReadError(format!("Failed to retrieve compiled class {class_hash:#}"))
            })?
        else {
            return Err(StateError::UndeclaredClassHash(class_hash));
        };

        to_blockifier_class(compiled_class).map_err(StateError::ProgramError)
    }

    fn get_compiled_class_hash(&self, class_hash: ClassHash) -> StateResult<CompiledClassHash> {
        log::debug!("get_compiled_class_hash for {:#?}", class_hash);

        let Some(on_top_of_block_id) = self.on_top_of_block_id else {
            return Err(StateError::UndeclaredClassHash(class_hash));
        };
        let Some(class_info) =
            self.backend.get_class_info(&on_top_of_block_id, &class_hash.to_felt()).map_err(|err| {
                log::warn!("Failed to retrieve compiled class hash {class_hash:#}: {err:#}");
                StateError::StateReadError(format!("Failed to retrieve compiled class hash {class_hash:#}",))
            })?
        else {
            return Err(StateError::UndeclaredClassHash(class_hash));
        };

        Ok(CompiledClassHash(class_info.compiled_class_hash))
    }
}

fn block_hash_storage_check_range(chain_id: &ChainId, current_block: u64, to_check: u64) -> bool {
    // Allowed range is first_v0_12_0_block..=(current_block - 10).
    let first_block = if chain_id == &ChainId::Mainnet { 103_129 } else { 0 };

    #[allow(clippy::reversed_empty_ranges)]
    current_block.checked_sub(10).map(|end| first_block..=end).unwrap_or(1..=0).contains(&to_check)
}

#[cfg(test)]
mod tests {
    use starknet_api::core::ChainId;

    use super::block_hash_storage_check_range;

    #[test]
    fn check_block_n_range() {
        let chain_id = ChainId::Other("MADARA_TEST".into());
        assert!(!block_hash_storage_check_range(&chain_id, 9, 0));
        assert!(block_hash_storage_check_range(&chain_id, 10, 0));
        assert!(block_hash_storage_check_range(&chain_id, 11, 0));
        assert!(!block_hash_storage_check_range(&chain_id, 50 + 9, 50));
        assert!(block_hash_storage_check_range(&chain_id, 50 + 10, 50));
        assert!(block_hash_storage_check_range(&chain_id, 50 + 11, 50));
        assert!(!block_hash_storage_check_range(&ChainId::Mainnet, 50 + 11, 50));
    }
}
