//    Copyright 2023 The Tari Project
//    SPDX-License-Identifier: BSD-3-Clause

use indexmap::IndexSet;
use log::*;
use tari_dan_app_utilities::transaction_executor::{TransactionExecutor, TransactionProcessorError};
use tari_dan_common_types::Epoch;
use tari_dan_engine::{
    bootstrap_state,
    state_store::{memory::MemoryStateStore, AtomicDb, StateWriter},
};
use tari_dan_storage::consensus_models::{ExecutedTransaction, SubstateLockFlag, VersionedSubstateIdLockIntent};
use tari_transaction::{Transaction, VersionedSubstateId};
use tokio::task;

use crate::{
    p2p::services::mempool::{MempoolError, SubstateResolver},
    substate_resolver::SubstateResolverError,
};

const LOG_TARGET: &str = "tari::dan::mempool::executor";

pub async fn execute_transaction<TSubstateResolver, TExecutor>(
    transaction: Transaction,
    substate_resolver: TSubstateResolver,
    executor: TExecutor,
    current_epoch: Epoch,
) -> Result<Result<ExecutedTransaction, MempoolError>, MempoolError>
where
    TSubstateResolver: SubstateResolver<Error = SubstateResolverError>,
    TExecutor: TransactionExecutor<Error = TransactionProcessorError> + Send + Sync + 'static,
{
    let virtual_substates = match substate_resolver
        .resolve_virtual_substates(&transaction, current_epoch)
        .await
    {
        Ok(virtual_substates) => virtual_substates,
        Err(err @ SubstateResolverError::UnauthorizedFeeClaim { .. }) => {
            warn!(target: LOG_TARGET, "One or more invalid fee claims for transaction {}: {}", transaction.id(), err);
            return Ok(Err(err.into()));
        },
        Err(err) => return Err(err.into()),
    };

    info!(target: LOG_TARGET, "Transaction {} executing. virtual_substates = [{}]", transaction.id(), virtual_substates.keys().map(|addr| addr.to_string()).collect::<Vec<_>>().join(", "));

    match substate_resolver.resolve(&transaction).await {
        Ok(inputs) => {
            let res = task::spawn_blocking(move || {
                let versioned_inputs = inputs
                    .iter()
                    .map(|(id, substate)| VersionedSubstateId::new(id.clone(), substate.version()))
                    .collect::<IndexSet<_>>();
                let state_db = new_state_db();
                state_db.set_many(inputs).expect("memory db is infallible");

                match executor.execute(transaction, state_db, virtual_substates) {
                    Ok(mut executed) => {
                        // Update the resolved inputs to set the specific version, as we know it after execution
                        let mut resolved_inputs = IndexSet::new();
                        if let Some(diff) = executed.result().finalize.accept() {
                            resolved_inputs = versioned_inputs
                                .into_iter()
                                .map(|versioned_id| {
                                    let lock_flag = if diff.down_iter().any(|(id, _)| *id == versioned_id.substate_id) {
                                        // Update all inputs that were DOWNed to be write locked
                                        SubstateLockFlag::Write
                                    } else {
                                        // Any input not downed, gets a read lock
                                        SubstateLockFlag::Read
                                    };
                                    VersionedSubstateIdLockIntent::new(versioned_id, lock_flag)
                                })
                                .collect::<IndexSet<_>>();
                        }

                        executed.set_resolved_inputs(resolved_inputs);
                        Ok(executed)
                    },
                    Err(err) => Err(err.into()),
                }
            })
            .await;

            // If this errors, the thread panicked due to a bug
            res.map_err(|err| MempoolError::ExecutionThreadFailure(err.to_string()))
        },
        // Substates are downed/dont exist
        Err(err @ SubstateResolverError::InputSubstateDowned { .. }) |
        Err(err @ SubstateResolverError::InputSubstateDoesNotExist { .. }) => {
            warn!(target: LOG_TARGET, "One or more invalid input shards for transaction {}: {}", transaction.id(), err);
            // Ok(Err(_)) This is not a mempool execution failure, but rather a transaction failure
            Ok(Err(err.into()))
        },
        // Some other issue - network, db, etc
        Err(err) => Err(err.into()),
    }
}

fn new_state_db() -> MemoryStateStore {
    let state_db = MemoryStateStore::new();
    // unwrap: Memory state store is infallible
    let mut tx = state_db.write_access().unwrap();
    // Add bootstrapped substates
    bootstrap_state(&mut tx).unwrap();
    tx.commit().unwrap();
    state_db
}
