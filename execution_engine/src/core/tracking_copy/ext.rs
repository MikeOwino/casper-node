use std::convert::TryInto;

use casper_storage::global_state::{
    shared::CorrelationId,
    storage::{state::StateReader, trie::merkle_proof::TrieMerkleProof},
};

use crate::ACCOUNT_WASM_ADDR;
use casper_types::contracts::{
    ContractPackageKind, ContractPackageStatus, ContractVersions, DisabledVersions, Groups,
};
use casper_types::{
    contracts::AccountHash, AccessRights, AddressableEntity, ByteCode, CLValue, ContractHash,
    ContractPackageHash, ContractWasmHash, EntryPoints, Key, Motes, Package, Phase,
    ProtocolVersion, StoredValue, StoredValueTypeMismatch, URef,
};

use crate::core::execution::AddressGenerator;
use crate::core::{
    engine_state::{ChecksumRegistry, SystemContractRegistry},
    execution,
    tracking_copy::TrackingCopy,
};

/// Higher-level operations on the state via a `TrackingCopy`.
pub trait TrackingCopyExt<R> {
    /// The type for the returned errors.
    type Error;

    /// Gets the contract hash for the account at a given account address.
    fn get_account(
        &mut self,
        correlation_id: CorrelationId,
        account_hash: AccountHash,
    ) -> Result<ContractHash, Self::Error>;

    /// Gets the contract for a given account by its account address
    fn get_contract_by_account_hash(
        &mut self,
        correlation_id: CorrelationId,
        protocol_version: ProtocolVersion,
        account_hash: AccountHash,
    ) -> Result<AddressableEntity, Self::Error>;

    /// Reads the contract hash for the account at a given account address.
    fn read_account(
        &mut self,
        correlation_id: CorrelationId,
        account_hash: AccountHash,
    ) -> Result<Key, Self::Error>;

    /// Reads the contract for a given account by its account address
    fn read_contract_by_account_hash(
        &mut self,
        correlation_id: CorrelationId,
        protocol_version: ProtocolVersion,
        account_hash: AccountHash,
    ) -> Result<AddressableEntity, Self::Error>;

    // TODO: make this a static method
    /// Gets the purse balance key for a given purse id.
    fn get_purse_balance_key(
        &self,
        correlation_id: CorrelationId,
        purse_key: Key,
    ) -> Result<Key, Self::Error>;

    /// Gets the balance at a given balance key.
    fn get_purse_balance(
        &self,
        correlation_id: CorrelationId,
        balance_key: Key,
    ) -> Result<Motes, Self::Error>;

    /// Gets the purse balance key for a given purse id and provides a Merkle proof.
    fn get_purse_balance_key_with_proof(
        &self,
        correlation_id: CorrelationId,
        purse_key: Key,
    ) -> Result<(Key, TrieMerkleProof<Key, StoredValue>), Self::Error>;

    /// Gets the balance at a given balance key and provides a Merkle proof.
    fn get_purse_balance_with_proof(
        &self,
        correlation_id: CorrelationId,
        balance_key: Key,
    ) -> Result<(Motes, TrieMerkleProof<Key, StoredValue>), Self::Error>;

    /// Gets a contract by Key.
    fn get_contract_wasm(
        &mut self,
        correlation_id: CorrelationId,
        contract_wasm_hash: ContractWasmHash,
    ) -> Result<ByteCode, Self::Error>;

    /// Gets a contract header by Key.
    fn get_contract(
        &mut self,
        correlation_id: CorrelationId,
        contract_hash: ContractHash,
    ) -> Result<AddressableEntity, Self::Error>;

    /// Gets a contract package by Key.
    fn get_contract_package(
        &mut self,
        correlation_id: CorrelationId,
        contract_package_hash: ContractPackageHash,
    ) -> Result<Package, Self::Error>;

    /// Gets the system contract registry.
    fn get_system_contracts(
        &mut self,
        correlation_id: CorrelationId,
    ) -> Result<SystemContractRegistry, Self::Error>;

    /// Gets the system checksum registry.
    fn get_checksum_registry(
        &mut self,
        correlation_id: CorrelationId,
    ) -> Result<Option<ChecksumRegistry>, Self::Error>;
}

impl<R> TrackingCopyExt<R> for TrackingCopy<R>
where
    R: StateReader<Key, StoredValue>,
    R::Error: Into<execution::Error>,
{
    type Error = execution::Error;

    fn get_account(
        &mut self,
        correlation_id: CorrelationId,
        account_hash: AccountHash,
    ) -> Result<ContractHash, Self::Error> {
        let account_key = Key::Account(account_hash);
        match self.get(correlation_id, &account_key).map_err(Into::into)? {
            Some(StoredValue::CLValue(cl_value)) => {
                let contract_hash = CLValue::into_t::<Key>(cl_value)?;
                let contract_hash = contract_hash
                    .into_hash()
                    .map(ContractHash::new)
                    .expect("must convert to contract hash");

                Ok(contract_hash)
            }
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("CLValue".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(account_key)),
        }
    }

    fn get_contract_by_account_hash(
        &mut self,
        correlation_id: CorrelationId,
        protocol_version: ProtocolVersion,
        account_hash: AccountHash,
    ) -> Result<AddressableEntity, Self::Error> {
        let account_key = Key::Account(account_hash);

        let contract_key = match self.get(correlation_id, &account_key).map_err(Into::into)? {
            Some(StoredValue::CLValue(contract_key_as_cl_value)) => {
                CLValue::into_t::<Key>(contract_key_as_cl_value)?
            }
            Some(StoredValue::Account(account)) => {
                let mut generator =
                    AddressGenerator::new(account.main_purse().addr().as_ref(), Phase::System);

                let contract_wasm_hash = ContractWasmHash::new(ACCOUNT_WASM_ADDR);
                let contract_hash = ContractHash::new(generator.new_hash_address());
                let contract_package_hash = ContractPackageHash::new(generator.new_hash_address());

                let entry_points = EntryPoints::new();

                let entity = AddressableEntity::new(
                    contract_package_hash,
                    contract_wasm_hash,
                    account.named_keys().clone(),
                    entry_points,
                    protocol_version,
                    account.main_purse(),
                    account.associated_keys().clone(),
                    account.action_thresholds().clone(),
                );

                let access_key = generator.new_uref(AccessRights::READ_ADD_WRITE);

                let contract_package = {
                    let mut contract_package = Package::new(
                        access_key,
                        ContractVersions::default(),
                        DisabledVersions::default(),
                        Groups::default(),
                        ContractPackageStatus::Locked,
                        ContractPackageKind::Account(account_hash),
                    );
                    contract_package
                        .insert_contract_version(protocol_version.value().major, contract_hash);
                    contract_package
                };

                let contract_key: Key = contract_hash.into();

                self.write(contract_key, StoredValue::AddressableEntity(entity.clone()));
                self.write(contract_package_hash.into(), contract_package.into());

                let contract_by_account = match CLValue::from_t(contract_key) {
                    Ok(cl_value) => cl_value,
                    Err(error) => return Err(execution::Error::CLValue(error)),
                };

                self.write(account_key, StoredValue::CLValue(contract_by_account));

                return Ok(entity);
            }

            Some(other) => {
                return Err(execution::Error::TypeMismatch(
                    StoredValueTypeMismatch::new("Key".to_string(), other.type_name()),
                ));
            }
            None => return Err(execution::Error::KeyNotFound(account_key)),
        };
        match self
            .get(correlation_id, &contract_key)
            .map_err(Into::into)?
        {
            Some(StoredValue::AddressableEntity(contract)) => Ok(contract),
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("Contract".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(contract_key)),
        }
    }

    fn read_account(
        &mut self,
        correlation_id: CorrelationId,
        account_hash: AccountHash,
    ) -> Result<Key, Self::Error> {
        let account_key = Key::Account(account_hash);
        match self
            .read(correlation_id, &account_key)
            .map_err(Into::into)?
        {
            Some(StoredValue::CLValue(cl_value)) => Ok(CLValue::into_t(cl_value)?),
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("Account".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(account_key)),
        }
    }

    fn read_contract_by_account_hash(
        &mut self,
        correlation_id: CorrelationId,
        protocol_version: ProtocolVersion,
        account_hash: AccountHash,
    ) -> Result<AddressableEntity, Self::Error> {
        self.get_contract_by_account_hash(correlation_id, protocol_version, account_hash)
    }

    fn get_purse_balance_key(
        &self,
        _correlation_id: CorrelationId,
        purse_key: Key,
    ) -> Result<Key, Self::Error> {
        let balance_key: URef = purse_key
            .into_uref()
            .ok_or(execution::Error::KeyIsNotAURef(purse_key))?;
        Ok(Key::Balance(balance_key.addr()))
    }

    fn get_purse_balance(
        &self,
        correlation_id: CorrelationId,
        key: Key,
    ) -> Result<Motes, Self::Error> {
        let stored_value: StoredValue = self
            .read(correlation_id, &key)
            .map_err(Into::into)?
            .ok_or(execution::Error::KeyNotFound(key))?;
        let cl_value: CLValue = stored_value
            .try_into()
            .map_err(execution::Error::TypeMismatch)?;
        let balance = Motes::new(cl_value.into_t()?);
        Ok(balance)
    }

    fn get_purse_balance_key_with_proof(
        &self,
        correlation_id: CorrelationId,
        purse_key: Key,
    ) -> Result<(Key, TrieMerkleProof<Key, StoredValue>), Self::Error> {
        let balance_key: Key = purse_key
            .uref_to_hash()
            .ok_or(execution::Error::KeyIsNotAURef(purse_key))?;
        let proof: TrieMerkleProof<Key, StoredValue> = self
            .read_with_proof(correlation_id, &balance_key) // Key::Hash, so no need to normalize
            .map_err(Into::into)?
            .ok_or(execution::Error::KeyNotFound(purse_key))?;
        let stored_value_ref: &StoredValue = proof.value();
        let cl_value: CLValue = stored_value_ref
            .to_owned()
            .try_into()
            .map_err(execution::Error::TypeMismatch)?;
        let balance_key: Key = cl_value.into_t()?;
        Ok((balance_key, proof))
    }

    fn get_purse_balance_with_proof(
        &self,
        correlation_id: CorrelationId,
        key: Key,
    ) -> Result<(Motes, TrieMerkleProof<Key, StoredValue>), Self::Error> {
        let proof: TrieMerkleProof<Key, StoredValue> = self
            .read_with_proof(correlation_id, &key.normalize())
            .map_err(Into::into)?
            .ok_or(execution::Error::KeyNotFound(key))?;
        let cl_value: CLValue = proof
            .value()
            .to_owned()
            .try_into()
            .map_err(execution::Error::TypeMismatch)?;
        let balance = Motes::new(cl_value.into_t()?);
        Ok((balance, proof))
    }

    /// Gets a contract wasm by Key
    fn get_contract_wasm(
        &mut self,
        correlation_id: CorrelationId,
        contract_wasm_hash: ContractWasmHash,
    ) -> Result<ByteCode, Self::Error> {
        let key = contract_wasm_hash.into();
        match self.get(correlation_id, &key).map_err(Into::into)? {
            Some(StoredValue::ContractWasm(contract_wasm)) => Ok(contract_wasm),
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("ContractWasm".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(key)),
        }
    }

    /// Gets a contract header by Key
    fn get_contract(
        &mut self,
        correlation_id: CorrelationId,
        contract_hash: ContractHash,
    ) -> Result<AddressableEntity, Self::Error> {
        let key = contract_hash.into();

        match self.read(correlation_id, &key).map_err(Into::into)? {
            Some(StoredValue::AddressableEntity(entity)) => Ok(entity),
            Some(StoredValue::Contract(contract)) => {
                let contract_key: Key = contract_hash.into();
                let entity: AddressableEntity = contract.into();
                self.write(contract_key, StoredValue::AddressableEntity(entity.clone()));
                Ok(entity)
            }
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new(
                    "AddressableEntity or Contract".to_string(),
                    other.type_name(),
                ),
            )),
            None => Err(execution::Error::KeyNotFound(key)),
        }
    }

    fn get_contract_package(
        &mut self,
        correlation_id: CorrelationId,
        contract_package_hash: ContractPackageHash,
    ) -> Result<Package, Self::Error> {
        let key = contract_package_hash.into();
        match self.read(correlation_id, &key).map_err(Into::into)? {
            Some(StoredValue::ContractPackage(contract_package)) => Ok(contract_package),
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("ContractPackage".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(key)),
        }
    }

    fn get_system_contracts(
        &mut self,
        correlation_id: CorrelationId,
    ) -> Result<SystemContractRegistry, Self::Error> {
        match self
            .get(correlation_id, &Key::SystemContractRegistry)
            .map_err(Into::into)?
        {
            Some(StoredValue::CLValue(registry)) => {
                let registry: SystemContractRegistry =
                    CLValue::into_t(registry).map_err(Self::Error::from)?;
                Ok(registry)
            }
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("CLValue".to_string(), other.type_name()),
            )),
            None => Err(execution::Error::KeyNotFound(Key::SystemContractRegistry)),
        }
    }

    fn get_checksum_registry(
        &mut self,
        correlation_id: CorrelationId,
    ) -> Result<Option<ChecksumRegistry>, Self::Error> {
        match self
            .get(correlation_id, &Key::ChecksumRegistry)
            .map_err(Into::into)?
        {
            Some(StoredValue::CLValue(registry)) => {
                let registry: ChecksumRegistry =
                    CLValue::into_t(registry).map_err(Self::Error::from)?;
                Ok(Some(registry))
            }
            Some(other) => Err(execution::Error::TypeMismatch(
                StoredValueTypeMismatch::new("CLValue".to_string(), other.type_name()),
            )),
            None => Ok(None),
        }
    }
}
