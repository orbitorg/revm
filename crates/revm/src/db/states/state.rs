use super::{
    cache::CacheState, plain_account::PlainStorage, BundleState, CacheAccount, TransitionState,
};
use crate::TransitionAccount;
use revm_interpreter::primitives::{
    db::{Database, DatabaseCommit},
    hash_map, Account, AccountInfo, Bytecode, HashMap, B160, B256, U256,
};

/// State of blockchain.
///
/// State clear flag is set inside CacheState and by default it is enabled.
/// If you want to disable it use `set_state_clear_flag` function.
pub struct State<'a, DBError> {
    /// Cached state contains both changed from evm execution and cached/loaded account/storages
    /// from database. This allows us to have only one layer of cache where we can fetch data.
    /// Additionaly we can introduce some preloading of data from database.
    pub cache: CacheState,
    /// Optional database that we use to fetch data from. If database is not present, we will
    /// return not existing account and storage.
    ///
    /// Note: It is marked as Send so database can be shared between threads.
    pub database: Box<dyn Database<Error = DBError> + Send + 'a>,
    /// Block state, it aggregates transactions transitions into one state.
    ///
    /// Build reverts and state that gets applied to the state.
    pub transition_state: Option<TransitionState>,
    /// After block is finishes we merge those changes inside bundle.
    /// Bundle is used to update database and create changesets.
    ///
    /// Bundle state can be present if we want to use preloaded bundle.
    pub bundle_state: Option<BundleState>,
    /// Addition layer that is going to be used to fetched values before fetching values
    /// from database.
    ///
    /// Bundle is the main output of the state execution and this allows setting previous bundle
    /// and using its values for execution.
    pub use_preloaded_bundle: bool,
    // if enabled USE Background thread for transitions and bundle
    //pub use_background_thread: bool,
}

impl<'a, DBError> State<'a, DBError> {
    /// Iterate over received balances and increment all account balances.
    /// If account is not found inside cache state it will be loaded from database.
    ///
    /// Update will create transitions for all accounts that are updated.
    pub fn increment_balances(
        &mut self,
        balances: impl IntoIterator<Item = (B160, u128)>,
    ) -> Result<(), DBError> {
        // make transition and update cache state
        let mut transitions = Vec::new();
        for (address, balance) in balances {
            let original_account = self.load_cache_account(address)?;
            transitions.push((address, original_account.increment_balance(balance)))
        }
        // append transition
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
        Ok(())
    }

    /// Drain balances from given account and return those values.
    ///
    /// It is used for DAO hardfork state change to move values from given accounts.
    pub fn drain_balances(
        &mut self,
        addresses: impl IntoIterator<Item = B160>,
    ) -> Result<Vec<u128>, DBError> {
        // make transition and update cache state
        let mut transitions = Vec::new();
        let mut balances = Vec::new();
        for address in addresses {
            let original_account = self.load_cache_account(address)?;
            let (balance, transition) = original_account.drain_balance();
            balances.push(balance);
            transitions.push((address, transition))
        }
        // append transition
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
        Ok(balances)
    }

    /// State clear EIP-161 is enabled in Spurious Dragon hardfork.
    pub fn set_state_clear_flag(&mut self, has_state_clear: bool) {
        self.cache.set_state_clear_flag(has_state_clear);
    }

    pub fn insert_not_existing(&mut self, address: B160) {
        self.cache.insert_not_existing(address)
    }

    pub fn insert_account(&mut self, address: B160, info: AccountInfo) {
        self.cache.insert_account(address, info)
    }

    pub fn insert_account_with_storage(
        &mut self,
        address: B160,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        self.cache
            .insert_account_with_storage(address, info, storage)
    }

    /// Apply evm transitions to transition state.
    fn apply_transition(&mut self, transitions: Vec<(B160, TransitionAccount)>) {
        // add transition to transition state.
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
    }

    /// Take all transitions and merge them inside bundle state.
    /// This action will create final post state and all reverts so that
    /// we at any time revert state of bundle to the state before transition
    /// is applied.
    pub fn merge_transitions(&mut self) {
        if let Some(transition_state) = self.transition_state.as_mut() {
            let transition_state = transition_state.take();

            self.bundle_state
                .get_or_insert(BundleState::default())
                .apply_block_substate_and_create_reverts(transition_state);
        }
    }

    pub fn load_cache_account(&mut self, address: B160) -> Result<&mut CacheAccount, DBError> {
        match self.cache.accounts.entry(address) {
            hash_map::Entry::Vacant(entry) => {
                let info = self.database.basic(address)?;
                let bundle_account = match info {
                    None => CacheAccount::new_loaded_not_existing(),
                    Some(acc) if acc.is_empty() => {
                        CacheAccount::new_loaded_empty_eip161(HashMap::new())
                    }
                    Some(acc) => CacheAccount::new_loaded(acc, HashMap::new()),
                };
                Ok(entry.insert(bundle_account))
            }
            hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
        }
    }

    /// Takes changeset and reverts from state and replaces it with empty one.
    /// This will trop pending Transition and any transitions would be lost.
    ///
    /// TODO make cache aware of transitions dropping by having global transition counter.
    pub fn take_bundle(&mut self) -> BundleState {
        std::mem::take(self.bundle_state.as_mut().unwrap())
    }
}

impl<'a, DBError> Database for State<'a, DBError> {
    type Error = DBError;

    fn basic(&mut self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        self.load_cache_account(address).map(|a| a.account_info())
    }

    fn code_by_hash(
        &mut self,
        code_hash: revm_interpreter::primitives::B256,
    ) -> Result<Bytecode, Self::Error> {
        let res = match self.cache.contracts.entry(code_hash) {
            hash_map::Entry::Occupied(entry) => Ok(entry.get().clone()),
            hash_map::Entry::Vacant(entry) => {
                let code = self.database.code_by_hash(code_hash)?;
                entry.insert(code.clone());
                Ok(code)
            }
        };
        res
    }

    fn storage(&mut self, address: B160, index: U256) -> Result<U256, Self::Error> {
        // Account is guaranteed to be loaded.
        if let Some(account) = self.cache.accounts.get_mut(&address) {
            // account will always be some, but if it is not, U256::ZERO will be returned.
            let is_storage_known = account.status.storage_known();
            Ok(account
                .account
                .as_mut()
                .map(|account| match account.storage.entry(index) {
                    hash_map::Entry::Occupied(entry) => Ok(*entry.get()),
                    hash_map::Entry::Vacant(entry) => {
                        // if account was destroyed or account is newly built
                        // we return zero and don't ask database.
                        let value = if is_storage_known {
                            U256::ZERO
                        } else {
                            self.database.storage(address, index)?
                        };
                        entry.insert(value);
                        Ok(value)
                    }
                })
                .transpose()?
                .unwrap_or_default())
        } else {
            unreachable!("For accessing any storage account is guaranteed to be loaded beforehand")
        }
    }

    fn block_hash(&mut self, number: U256) -> Result<B256, Self::Error> {
        // TODO maybe cache it.
        self.database.block_hash(number)
    }
}

impl<'a, DBError> DatabaseCommit for State<'a, DBError> {
    fn commit(&mut self, evm_state: HashMap<B160, Account>) {
        let transitions = self.cache.apply_evm_state(evm_state);
        self.apply_transition(transitions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{
            states::reverts::AccountInfoRevert, AccountRevert, AccountStatus, BundleAccount,
            RevertToSlot,
        },
        StateBuilder,
    };
    use revm_interpreter::primitives::StorageSlot;

    /// Checks that if accounts is touched multiple times in the same block,
    /// then the old values from the first change are preserved and not overwritten.
    ///
    /// This is important because the state transitions from different transactions in the same block may see
    /// different states of the same account as the old value, but the revert should reflect the
    /// state of the account before the block.
    #[test]
    fn account_and_storage_reverts_preserve_old_values() {
        let mut state = StateBuilder::default().build();

        let (slot1, slot2, slot3) = (U256::from(1), U256::from(2), U256::from(3));

        // Non-existing account for testing account state transitions.
        // [LoadedNotExisting] -> [Changed] (nonce: 1, balance: 1) -> [Changed] (nonce: 2) -> [Changed] (nonce: 3)
        let new_account_address = B160::from_slice(&[0x1; 20]);
        let new_account_created_info = AccountInfo {
            nonce: 1,
            balance: U256::from(1),
            ..Default::default()
        };
        let new_account_changed_info = AccountInfo {
            nonce: 2,
            ..new_account_created_info.clone()
        };
        let new_account_changed_info2 = AccountInfo {
            nonce: 3,
            ..new_account_changed_info.clone()
        };

        // Existing account for testing storage state transitions.
        let existing_account_address = B160::from_slice(&[0x2; 20]);
        let existing_account_initial_info = AccountInfo {
            nonce: 1,
            ..Default::default()
        };
        let existing_account_initial_storage = HashMap::<U256, U256>::from([
            (slot1, U256::from(100)), // 0x01 => 100
            (slot2, U256::from(200)), // 0x02 => 200
        ]);
        let existing_account_changed_info = AccountInfo {
            nonce: 2,
            ..existing_account_initial_info.clone()
        };

        // A transaction in block 1 creates one account and changes an existing one.
        state.apply_transition(Vec::from([
            (
                new_account_address,
                TransitionAccount {
                    status: AccountStatus::InMemoryChange,
                    info: Some(new_account_created_info.clone()),
                    previous_status: AccountStatus::LoadedNotExisting,
                    previous_info: None,
                    ..Default::default()
                },
            ),
            (
                existing_account_address,
                TransitionAccount {
                    status: AccountStatus::InMemoryChange,
                    info: Some(existing_account_changed_info.clone()),
                    previous_status: AccountStatus::Loaded,
                    previous_info: Some(existing_account_initial_info.clone()),
                    storage: HashMap::from([(
                        slot1,
                        StorageSlot {
                            previous_or_original_value: *existing_account_initial_storage
                                .get(&slot1)
                                .unwrap(),
                            present_value: U256::from(1000),
                        },
                    )]),
                    storage_was_destroyed: false,
                },
            ),
        ]));

        // A transaction in block 1 then changes the same account.
        state.apply_transition(Vec::from([(
            new_account_address,
            TransitionAccount {
                status: AccountStatus::InMemoryChange,
                info: Some(new_account_changed_info.clone()),
                previous_status: AccountStatus::InMemoryChange,
                previous_info: Some(new_account_created_info.clone()),
                ..Default::default()
            },
        )]));

        // Another transaction in block 1 then changes the newly created account yet again and modifies the storage in an existing one.
        state.apply_transition(Vec::from([
            (
                new_account_address,
                TransitionAccount {
                    status: AccountStatus::InMemoryChange,
                    info: Some(new_account_changed_info2.clone()),
                    previous_status: AccountStatus::InMemoryChange,
                    previous_info: Some(new_account_changed_info),
                    storage: HashMap::from([(
                        slot1,
                        StorageSlot {
                            previous_or_original_value: U256::ZERO,
                            present_value: U256::from(1),
                        },
                    )]),
                    storage_was_destroyed: false,
                },
            ),
            (
                existing_account_address,
                TransitionAccount {
                    status: AccountStatus::InMemoryChange,
                    info: Some(existing_account_changed_info.clone()),
                    previous_status: AccountStatus::InMemoryChange,
                    previous_info: Some(existing_account_changed_info.clone()),
                    storage: HashMap::from([
                        (
                            slot1,
                            StorageSlot {
                                previous_or_original_value: U256::from(100),
                                present_value: U256::from(1_000),
                            },
                        ),
                        (
                            slot2,
                            StorageSlot {
                                previous_or_original_value: *existing_account_initial_storage
                                    .get(&slot2)
                                    .unwrap(),
                                present_value: U256::from(2_000),
                            },
                        ),
                        // Create new slot
                        (
                            slot3,
                            StorageSlot {
                                previous_or_original_value: U256::ZERO,
                                present_value: U256::from(3_000),
                            },
                        ),
                    ]),
                    storage_was_destroyed: false,
                },
            ),
        ]));

        state.merge_transitions();
        let bundle_state = state.take_bundle();

        // The new account revert should be `DeleteIt` since this was an account creation.
        // The existing account revert should be reverted to its previous state.
        assert_eq!(
            bundle_state.reverts,
            Vec::from([Vec::from([
                (
                    new_account_address,
                    AccountRevert {
                        account: AccountInfoRevert::DeleteIt,
                        previous_status: AccountStatus::LoadedNotExisting,
                        storage: HashMap::from([(slot1, RevertToSlot::Some(U256::ZERO))]),
                        wipe_storage: false,
                    }
                ),
                (
                    existing_account_address,
                    AccountRevert {
                        account: AccountInfoRevert::RevertTo(existing_account_initial_info.clone()),
                        previous_status: AccountStatus::Loaded,
                        storage: HashMap::from([
                            (
                                slot1,
                                RevertToSlot::Some(
                                    *existing_account_initial_storage.get(&slot1).unwrap()
                                )
                            ),
                            (
                                slot2,
                                RevertToSlot::Some(
                                    *existing_account_initial_storage.get(&slot2).unwrap()
                                )
                            ),
                            (slot3, RevertToSlot::Some(U256::ZERO))
                        ]),
                        wipe_storage: false,
                    }
                ),
            ])]),
            "The account or storage reverts are incorrect"
        );

        // The latest state of the new account should be: nonce = 3, balance = 1, code & code hash = None.
        // Storage: 0x01 = 1.
        assert_eq!(
            bundle_state.account(&new_account_address),
            Some(&BundleAccount {
                info: Some(new_account_changed_info2),
                original_info: None,
                status: AccountStatus::InMemoryChange,
                storage: HashMap::from([(
                    slot1,
                    StorageSlot {
                        previous_or_original_value: U256::ZERO,
                        present_value: U256::from(1),
                    }
                )])
            }),
            "The latest state of the new account is incorrect"
        );

        // The latest state of the existing account should be: nonce = 2.
        // Storage: 0x01 = 1000, 0x02 = 2000, 0x03 = 3000.
        assert_eq!(
            bundle_state.account(&existing_account_address),
            Some(&BundleAccount {
                info: Some(existing_account_changed_info),
                original_info: Some(existing_account_initial_info),
                status: AccountStatus::InMemoryChange,
                storage: HashMap::from([
                    (
                        slot1,
                        StorageSlot {
                            previous_or_original_value: *existing_account_initial_storage
                                .get(&slot1)
                                .unwrap(),
                            present_value: U256::from(1_000),
                        },
                    ),
                    (
                        slot2,
                        StorageSlot {
                            previous_or_original_value: *existing_account_initial_storage
                                .get(&slot2)
                                .unwrap(),
                            present_value: U256::from(2_000),
                        },
                    ),
                    // Create new slot
                    (
                        slot3,
                        StorageSlot {
                            previous_or_original_value: U256::ZERO,
                            present_value: U256::from(3_000),
                        },
                    ),
                ])
            }),
            "The latest state of the existing account is incorrect"
        );
    }
}