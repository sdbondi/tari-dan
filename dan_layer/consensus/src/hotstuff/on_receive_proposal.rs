//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::ops::DerefMut;

use log::*;
use tari_dan_common_types::{
    committee::{Committee, CommitteeShard},
    optional::Optional,
    NodeHeight,
};
use tari_dan_storage::{
    consensus_models::{
        Block,
        BlockId,
        Command,
        Decision,
        ExecutedTransaction,
        LastExecuted,
        LastVoted,
        LockedBlock,
        QuorumDecision,
        SubstateLockFlag,
        SubstateRecord,
        TransactionPool,
        TransactionPoolStage,
    },
    StateStore,
    StateStoreReadTransaction,
    StateStoreWriteTransaction,
};
use tari_epoch_manager::EpochManagerReader;
use tari_transaction::Transaction;
use tokio::sync::{broadcast, mpsc};

use crate::{
    hotstuff::{
        common::update_high_qc,
        error::HotStuffError,
        event::HotstuffEvent,
        on_beat::OnBeat,
        ProposalValidationError,
    },
    messages::{HotstuffMessage, ProposalMessage, RequestMissingTransactionsMessage, VoteMessage},
    traits::{ConsensusSpec, LeaderStrategy, StateManager, VoteSignatureService},
};

const LOG_TARGET: &str = "tari::dan::consensus::hotstuff::on_receive_proposal";

pub struct OnReceiveProposalHandler<TConsensusSpec: ConsensusSpec> {
    validator_addr: TConsensusSpec::Addr,
    store: TConsensusSpec::StateStore,
    epoch_manager: TConsensusSpec::EpochManager,
    vote_signing_service: TConsensusSpec::VoteSignatureService,
    leader_strategy: TConsensusSpec::LeaderStrategy,
    state_manager: TConsensusSpec::StateManager,
    transaction_pool: TransactionPool<TConsensusSpec::StateStore>,
    tx_leader: mpsc::Sender<(TConsensusSpec::Addr, HotstuffMessage)>,
    tx_events: broadcast::Sender<HotstuffEvent>,
    on_beat: OnBeat,
}

impl<TConsensusSpec> OnReceiveProposalHandler<TConsensusSpec>
where TConsensusSpec: ConsensusSpec
{
    pub fn new(
        validator_addr: TConsensusSpec::Addr,
        store: TConsensusSpec::StateStore,
        epoch_manager: TConsensusSpec::EpochManager,
        vote_signing_service: TConsensusSpec::VoteSignatureService,
        leader_strategy: TConsensusSpec::LeaderStrategy,
        state_manager: TConsensusSpec::StateManager,
        transaction_pool: TransactionPool<TConsensusSpec::StateStore>,
        tx_leader: mpsc::Sender<(TConsensusSpec::Addr, HotstuffMessage)>,
        tx_events: broadcast::Sender<HotstuffEvent>,
        on_beat: OnBeat,
    ) -> Self {
        Self {
            validator_addr,
            store,
            epoch_manager,
            vote_signing_service,
            leader_strategy,
            state_manager,
            transaction_pool,
            tx_leader,
            tx_events,
            on_beat,
        }
    }

    pub async fn handle(&self, from: TConsensusSpec::Addr, message: ProposalMessage) -> Result<(), HotStuffError> {
        let ProposalMessage { block } = message;

        let local_committee = self.epoch_manager.get_local_committee(block.epoch()).await?;
        if local_committee.contains(&from) {
            debug!(
                target: LOG_TARGET,
                "🔥 Receive LOCAL PROPOSAL for block {}, parent {}, height {} from {}",
                block.id(),
                block.parent(),
                block.height(),
                from,
            );

            self.handle_local_proposal(from, local_committee, block).await
        } else {
            debug!(
                target: LOG_TARGET,
                "🔥 Receive FOREIGN PROPOSAL for block {}, parent {}, height {} from {}",
                block.id(),
                block.parent(),
                block.height(),
                from,
            );

            self.handle_foreign_proposal(from, block).await
        }
    }

    async fn block_has_missing_transaction(
        &self,
        local_committee: &Committee<TConsensusSpec::Addr>,
        block: &Block,
    ) -> Result<bool, HotStuffError> {
        let mut missing_tx_ids = Vec::new();
        self.store.with_read_tx(|tx| {
            for tx_id in block.all_transaction_ids() {
                if !ExecutedTransaction::exists(tx, tx_id)? {
                    missing_tx_ids.push(*tx_id);
                }
            }
            Ok::<_, HotStuffError>(())
        })?;
        if missing_tx_ids.is_empty() {
            Ok(false)
        } else {
            self.store
                .with_write_tx(|tx| tx.insert_missing_transactions(block.id(), missing_tx_ids.clone()))?;
            self.send_to_leader(
                local_committee,
                block.id(),
                HotstuffMessage::RequestMissingTransactions(RequestMissingTransactionsMessage {
                    block_id: *block.id(),
                    epoch: block.epoch(),
                    transactions: missing_tx_ids,
                }),
            )
            .await?;
            Ok(true)
        }
    }

    async fn handle_local_proposal(
        &self,
        from: TConsensusSpec::Addr,
        local_committee: Committee<TConsensusSpec::Addr>,
        block: Block,
    ) -> Result<(), HotStuffError> {
        // First save the block in one db transaction
        self.store.with_write_tx(|tx| {
            self.validate_local_proposed_block(&mut *tx, &from, &block)?;
            // Insert the block if it doesnt already exist
            block.justify().save(tx)?;
            block.save(tx)?;
            Ok::<_, HotStuffError>(())
        })?;

        if self.block_has_missing_transaction(&local_committee, &block).await? {
            Ok(())
        } else {
            self.process_block(&local_committee, &block).await
        }
    }

    pub async fn reprocess_block(&self, block_id: &BlockId) -> Result<(), HotStuffError> {
        let block = self.store.with_read_tx(|tx| Block::get(tx, block_id))?;
        let local_committee = self.epoch_manager.get_local_committee(block.epoch()).await?;
        self.process_block(&local_committee, &block).await
    }

    async fn process_block(
        &self,
        local_committee: &Committee<<TConsensusSpec as ConsensusSpec>::Addr>,
        block: &Block,
    ) -> Result<(), HotStuffError> {
        let local_committee_shard = self.epoch_manager.get_local_committee_shard(block.epoch()).await?;
        let maybe_decision = self.store.with_write_tx(|tx| {
            let should_vote = self.should_vote(&mut *tx, block)?;

            let mut maybe_decision = None;
            if should_vote {
                maybe_decision = self.decide_what_to_vote(tx, block, &local_committee_shard)?;
            }

            self.update_nodes(tx, block, &local_committee_shard)?;
            Ok::<_, HotStuffError>(maybe_decision)
        })?;

        if let Some(decision) = maybe_decision {
            let vote = self.generate_vote_message(block, decision).await?;
            debug!(
                target: LOG_TARGET,
                "🔥 Send {:?} VOTE for block {}, parent {}, height {}",
                decision,
                block.id(),
                block.parent(),
                block.height(),
            );
            self.send_vote_to_leader(local_committee, vote).await?;
        }

        Ok(())
    }

    async fn handle_foreign_proposal(&self, from: TConsensusSpec::Addr, block: Block) -> Result<(), HotStuffError> {
        let vn = self.epoch_manager.get_validator_node(block.epoch(), &from).await?;
        let committee_shard = self
            .epoch_manager
            .get_committee_shard(block.epoch(), vn.shard_key)
            .await?;
        self.validate_proposed_block(&from, &block)?;
        self.store
            .with_write_tx(|tx| self.on_receive_foreign_block(tx, &block, &committee_shard))?;

        // We could have ready transactions at this point, so if we're the leader for the next block we can propose
        self.on_beat.beat();

        Ok(())
    }

    fn on_receive_foreign_block(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        block: &Block,
        foreign_committee_shard: &CommitteeShard,
    ) -> Result<(), HotStuffError> {
        // Save the QCs if it doesnt exist already, we'll reference the QC in subsequent blocks
        block.justify().save(tx)?;

        // TODO(perf): n queries
        for cmd in block.commands() {
            let Some(t) = cmd.local_prepared() else {
                continue;
            };
            let Some(mut tx_rec) = self.transaction_pool.get(tx, &t.id).optional()? else {
                continue;
            };

            if tx_rec.stage().is_complete() {
                warn!(
                    target: LOG_TARGET,
                    "⚠️ Foreign proposal received after transaction {} is COMPLETE. Ignoring.",
                    tx_rec.transaction.id);
                continue;
            }

            tx_rec.update_evidence(tx, foreign_committee_shard, *block.justify().id())?;
            let change_to_abort = cmd.decision().is_abort() && tx_rec.original_decision().is_commit();
            if change_to_abort {
                info!(
                    target: LOG_TARGET,
                    "⚠️ Foreign shard ABORT {}. Update decision to ABORT",
                    tx_rec.transaction.id
                );
                tx_rec.update_decision(tx, Decision::Abort)?;
            }

            // If we've received we know that all locals have prepared and have all the evidence from all shards, we can
            // transition to All/SomePrepared
            if tx_rec.stage().is_local_prepared() && tx_rec.transaction.evidence.all_shards_complete() {
                if change_to_abort {
                    tx_rec.transition(tx, TransactionPoolStage::SomePrepared, true)?;
                } else {
                    tx_rec.transition(tx, TransactionPoolStage::AllPrepared, true)?;
                }
            }
        }

        Ok(())
    }

    async fn send_to_leader(
        &self,
        local_committee: &Committee<TConsensusSpec::Addr>,
        block_id: &BlockId,
        message: HotstuffMessage,
    ) -> Result<(), HotStuffError> {
        let leader = self.leader_strategy.get_leader(local_committee, block_id, 0);
        self.tx_leader
            .send((leader.clone(), message))
            .await
            .map_err(|_| HotStuffError::InternalChannelClosed {
                context: "tx_leader in OnReceiveProposalHandler::handle_local_proposal",
            })
    }

    async fn send_vote_to_leader(
        &self,
        local_committee: &Committee<TConsensusSpec::Addr>,
        vote: VoteMessage,
    ) -> Result<(), HotStuffError> {
        self.send_to_leader(local_committee, &vote.clone().block_id, HotstuffMessage::Vote(vote))
            .await
    }

    #[allow(clippy::too_many_lines)]
    fn decide_what_to_vote(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        block: &Block,
        local_committee_shard: &CommitteeShard,
    ) -> Result<Option<QuorumDecision>, HotStuffError> {
        block.as_last_voted().set(tx)?;

        for cmd in block.commands() {
            let mut tx_rec = self.transaction_pool.get(tx, cmd.transaction_id())?;
            // TODO: we probably need to provide the all/some of the QCs referenced in local transactions as
            //       part of the proposal DanMessage so that there is no race condition between receiving the
            //       AllProposed and receiving the foreign proposals
            tx_rec.update_evidence(tx, local_committee_shard, *block.justify().id())?;

            debug!(
                target: LOG_TARGET,
                "🔥 vote for block {} {}. Cmd: {}",
                block.id(),
                block.height(),
                cmd,
            );
            match cmd {
                Command::Prepare(t) => {
                    if !tx_rec.stage().is_new() {
                        warn!(
                            target: LOG_TARGET,
                            "❌ Stage disagreement for block {}. Leader proposed Prepare, local stage {}",
                            block.id(),
                            tx_rec.stage()
                        );
                        return Ok(None);
                    }
                    if tx_rec.original_decision() == t.decision {
                        if tx_rec.original_decision().is_commit() {
                            let transaction = ExecutedTransaction::get(tx.deref_mut(), cmd.transaction_id())?;
                            // Lock all inputs for the transaction as part of LocalPrepare
                            if !self.lock_inputs(tx, transaction.transaction(), local_committee_shard)? {
                                // Unable to lock all inputs - abstain? or vote reject?
                                warn!(
                                    target: LOG_TARGET,
                                    "❌ Unable to lock inputs for block {}. Leader proposed {}, we decided {}",
                                    block.id(),
                                    t.decision,
                                    tx_rec.original_decision()
                                );
                                return Ok(None);
                            }
                        }

                        tx_rec.transition(tx, TransactionPoolStage::Prepared, true)?;
                    } else {
                        // If we disagree with any local decision we abstain from voting
                        warn!(
                            target: LOG_TARGET,
                            "❌ Prepare decision disagreement for block {}. Leader proposed {}, we decided {}",
                            block.id(),
                            t.decision,
                            tx_rec.original_decision()
                        );
                        return Ok(None);
                    }
                },
                Command::LocalPrepared(t) => {
                    if tx_rec.stage().is_new() {
                        warn!(
                            target: LOG_TARGET,
                            "❌ Stage disagreement in block {} for transaction {}. Leader proposed LocalPrepared, but we have not prepared",
                            block.id(),
                            tx_rec.transaction_id()
                        );
                        return Ok(None);
                    }
                    // We check that the committee decision is different from the local decision.
                    // If the decision was changed (due to a foreign ABORT), we may have a disagreement so check both
                    if tx_rec.original_decision() != t.decision &&
                        tx_rec.changed_decision().map(|d| d != t.decision).unwrap_or(true)
                    {
                        warn!(
                            target: LOG_TARGET,
                            "❌ LocalPrepared decision disagreement for block {}. Leader proposed {}, we decided {}",
                            block.id(),
                            t.decision,
                            tx_rec.transaction.decision
                        );
                        return Ok(None);
                    }

                    if tx_rec.stage().is_prepared() {
                        tx_rec.transition(tx, TransactionPoolStage::LocalPrepared, false)?;
                    }

                    if tx_rec.transaction.evidence.all_shards_complete() {
                        if tx_rec.changed_decision().map(|d| d.is_abort()).unwrap_or(false) {
                            warn!(
                                target: LOG_TARGET,
                                "⚠️ LocalPrepared({}): Decision changed to ABORT", tx_rec.transaction_id()
                            );
                            tx_rec.transition(tx, TransactionPoolStage::SomePrepared, true)?;
                        } else {
                            tx_rec.transition(tx, TransactionPoolStage::AllPrepared, true)?;
                        }
                    }
                },
                Command::Accept(t) => {
                    if !tx_rec.stage().is_all_prepared() && !tx_rec.stage().is_some_prepared() {
                        warn!(
                            target: LOG_TARGET,
                            "❌ Stage disagreement for block {}. Leader proposed Accept, local stage {}",
                            block.id(),
                            tx_rec.stage()
                        );
                        return Ok(None);
                    }
                    if tx_rec.final_decision() != t.decision {
                        warn!(
                            target: LOG_TARGET,
                            "❌ Accept decision disagreement for block {}. Leader proposed {}, we decided {}",
                            block.id(),
                            t.decision,
                            tx_rec.final_decision()
                        );
                        return Ok(None);
                    }
                    tx_rec.transition(tx, TransactionPoolStage::Complete, false)?;
                },
            }
        }

        info!(target: LOG_TARGET, "✅ Accepting block {}", block.id());
        Ok(Some(QuorumDecision::Accept))
    }

    fn lock_inputs(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        transaction: &Transaction,
        local_committee_shard: &CommitteeShard,
    ) -> Result<bool, HotStuffError> {
        let state = SubstateRecord::try_lock_many(
            tx,
            transaction.id(),
            local_committee_shard.filter(transaction.inputs().iter().chain(transaction.filled_inputs())),
            SubstateLockFlag::Write,
        )?;
        if !state.is_acquired() {
            return Ok(false);
        }
        let state = SubstateRecord::try_lock_many(
            tx,
            transaction.id(),
            local_committee_shard.filter(transaction.input_refs()),
            SubstateLockFlag::Read,
        )?;

        if !state.is_acquired() {
            return Ok(false);
        }

        Ok(true)
    }

    fn unlock_inputs(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        transaction: &Transaction,
        local_committee_shard: &CommitteeShard,
    ) -> Result<(), HotStuffError> {
        SubstateRecord::try_unlock_many(
            tx,
            transaction.id(),
            local_committee_shard.filter(transaction.inputs().iter().chain(transaction.filled_inputs())),
            SubstateLockFlag::Write,
        )?;
        SubstateRecord::try_unlock_many(
            tx,
            transaction.id(),
            local_committee_shard.filter(transaction.input_refs()),
            SubstateLockFlag::Read,
        )?;
        Ok(())
    }

    async fn generate_vote_message(
        &self,
        block: &Block,
        decision: QuorumDecision,
    ) -> Result<VoteMessage, HotStuffError> {
        let merkle_proof = self
            .epoch_manager
            .get_validator_node_merkle_proof(block.epoch())
            .await?;
        let vn = self
            .epoch_manager
            .get_validator_node(block.epoch(), &self.validator_addr)
            .await?;
        let leaf_hash = vn.node_hash();

        let signature = self.vote_signing_service.sign_vote(&leaf_hash, block.id(), &decision);

        Ok(VoteMessage {
            epoch: block.epoch(),
            block_id: *block.id(),
            decision,
            signature,
            merkle_proof,
        })
    }

    fn update_nodes(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        block: &Block,
        local_committee_shard: &CommitteeShard,
    ) -> Result<(), HotStuffError> {
        update_high_qc(tx, block.justify())?;

        // b'' <- b*.justify.node
        let Some(commit_node) = block.justify().get_block(tx.deref_mut()).optional()? else {
            return Ok(());
        };

        // b' <- b''.justify.node
        let Some(precommit_node) = commit_node.justify().get_block(tx.deref_mut()).optional()? else {
            return Ok(());
        };

        let locked_block = LockedBlock::get(tx.deref_mut(), block.epoch())?;
        if precommit_node.height() > locked_block.height {
            debug!(target: LOG_TARGET, "LOCKED NODE SET: {}", precommit_node.id());
            // precommit_node is at COMMIT phase
            precommit_node.as_locked().set(tx)?;
        }

        // b <- b'.justify.node
        let prepare_node = precommit_node.justify().block_id();
        if commit_node.parent() == precommit_node.id() && precommit_node.parent() == prepare_node {
            debug!(
                target: LOG_TARGET,
                "✅ Node {} forms a 3-chain b'' = {}, b' = {}, b = {}",
                block.id(),
                commit_node.id(),
                precommit_node.id(),
                prepare_node,
            );

            let last_executed = LastExecuted::get(tx.deref_mut(), block.epoch())?;
            self.on_commit(tx, &last_executed, block, local_committee_shard)?;
            block.as_last_executed().set(tx)?;
        } else {
            debug!(
                target: LOG_TARGET,
                "Node DOES NOT form a 3-chain b'' = {}, b' = {}, b = {}, b* = {}",
                commit_node.id(),
                precommit_node.id(),
                prepare_node,
                block.id()
            );
        }

        Ok(())
    }

    fn on_commit(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        last_executed: &LastExecuted,
        block: &Block,
        local_committee_shard: &CommitteeShard,
    ) -> Result<(), HotStuffError> {
        if last_executed.height < block.height() {
            let parent = block.get_parent(tx.deref_mut())?;
            // Recurse to "catch up" any parent parent blocks we may not have executed
            self.on_commit(tx, last_executed, &parent, local_committee_shard)?;
            self.execute(tx, block, local_committee_shard)?;
            self.publish_event(HotstuffEvent::BlockCommitted { block_id: *block.id() });
        }
        Ok(())
    }

    fn publish_event(&self, event: HotstuffEvent) {
        let _ignore = self.tx_events.send(event);
    }

    fn execute(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::WriteTransaction<'_>,
        block: &Block,
        local_committee_shard: &CommitteeShard,
    ) -> Result<(), HotStuffError> {
        for cmd in block.commands() {
            let tx_rec = self.transaction_pool.get(tx, cmd.transaction_id())?;
            match cmd {
                Command::Prepare(_t) => {},
                Command::LocalPrepared(_t) => {
                    // TODO: Check if it's ok to unlock the inputs for ABORT at this point
                },
                Command::Accept(t) => {
                    debug!(
                        target: LOG_TARGET,
                        "Transaction {} is finalized ({})", tx_rec.transaction.id, t.decision
                    );
                    let mut executed = t.get_transaction(tx.deref_mut())?;
                    match t.decision {
                        // Commit the transaction substate changes.
                        Decision::Commit => {
                            self.state_manager
                                .commit_transaction(tx, block, &executed)
                                .map_err(|e| HotStuffError::StateManagerError(e.into()))?;

                            // We unlock just so that inputs that were not mutated are unlocked, even though those
                            // should be in input_refs
                            self.unlock_inputs(tx, executed.transaction(), local_committee_shard)?;
                        },
                        // Unlock the aborted inputs.
                        Decision::Abort => {
                            self.unlock_inputs(tx, executed.transaction(), local_committee_shard)?;
                        },
                    }

                    tx_rec.remove(tx)?;
                    executed.set_final_decision(t.decision).update(tx)?;
                },
            }
        }

        Ok(())
    }

    fn validate_local_proposed_block(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::ReadTransaction<'_>,
        from: &TConsensusSpec::Addr,
        candidate_block: &Block,
    ) -> Result<(), ProposalValidationError> {
        self.validate_proposed_block(from, candidate_block)?;

        // Check that details included in the justify match previously added blocks
        let Some(justify_block) = candidate_block.justify().get_block(tx).optional()? else {
            // TODO: This may mean that we have to catch up
            return Err(ProposalValidationError::JustifyBlockNotFound {
                proposed_by: from.to_string(),
                hash: *candidate_block.id(),
                justify_block: *candidate_block.justify().block_id(),
            });
        };

        if justify_block.height() != candidate_block.justify().block_height() {
            return Err(ProposalValidationError::JustifyBlockInvalid {
                proposed_by: from.to_string(),
                block_id: *candidate_block.id(),
                details: format!(
                    "Justify block height ({}) does not match justify block height ({})",
                    justify_block.height(),
                    candidate_block.justify().block_height()
                ),
            });
        }

        Ok(())
    }

    fn validate_proposed_block(
        &self,
        from: &TConsensusSpec::Addr,
        candidate_block: &Block,
    ) -> Result<(), ProposalValidationError> {
        if candidate_block.height() == NodeHeight::zero() || candidate_block.id().is_genesis() {
            return Err(ProposalValidationError::ProposingGenesisBlock {
                proposed_by: from.to_string(),
                hash: *candidate_block.id(),
            });
        }

        let calculated_hash = candidate_block.calculate_hash().into();
        if calculated_hash != *candidate_block.id() {
            return Err(ProposalValidationError::NodeHashMismatch {
                proposed_by: from.to_string(),
                hash: *candidate_block.id(),
                calculated_hash,
            });
        }

        // TODO: validate justify signatures
        // self.validate_qc(candidate_block.justify(), committee)?;

        Ok(())
    }

    /// if b_new .height > vheight && (b_new extends b_lock || b_new .justify.node.height > b_lock .height)
    ///
    /// If we have not previously voted on this block and the node extends the current locked node, then we vote
    fn should_vote(
        &self,
        tx: &mut <TConsensusSpec::StateStore as StateStore>::ReadTransaction<'_>,
        block: &Block,
    ) -> Result<bool, HotStuffError> {
        let Some(last_voted) = LastVoted::get(tx, block.epoch()).optional()? else {
            // Never voted, then validated.block.height() > last_voted.height (0)
            return Ok(true);
        };

        // if b_new .height > vheight And ...
        if block.height() <= last_voted.height {
            info!(
                target: LOG_TARGET,
                "❌ NOT voting on block {}, height {}. Block height is not greater than last voted height {}",
                block.id(),
                block.height(),
                last_voted.height,
            );
            return Ok(false);
        }

        let locked = LockedBlock::get(tx, block.epoch())?;
        let locked_block = locked.get_block(tx)?;

        // (b_new extends b_lock && b_new .justify.node.height > b_lock .height)
        if !is_safe_block(tx, block, &locked_block)? {
            info!(
                target: LOG_TARGET,
                "❌ NOT voting on block {}, height {}. Block does not satisfy safeNode predicate",
                block.id(),
                block.height(),
            );
            return Ok(false);
        }

        Ok(true)
    }
}

/// safeNode predicate (https://arxiv.org/pdf/1803.05069v6.pdf)
///
/// The safeNode predicate is a core ingredient of the protocol. It examines a proposal message
/// m carrying a QC justication m.justify, and determines whether m.node is safe to accept. The safety rule to accept
/// a proposal is the branch of m.node extends from the currently locked node lockedQC.node. On the other hand, the
/// liveness rule is the replica will accept m if m.justify has a higher view than the current lockedQC. The predicate
/// is true as long as either one of two rules holds.
fn is_safe_block<TTx: StateStoreReadTransaction>(
    tx: &mut TTx,
    block: &Block,
    locked_block: &Block,
) -> Result<bool, HotStuffError> {
    // Liveness
    if block.justify().block_height() <= locked_block.height() {
        debug!(
            target: LOG_TARGET,
            "❌ justify block height {} less than or equal to locked block height {}. Block does not satisfy safeNode predicate",
            block.justify().block_height(),
            locked_block.height(),
        );
        return Ok(false);
    }

    // Safety
    let extends = block.extends(tx, locked_block.id())?;
    if !extends {
        debug!(
            target: LOG_TARGET,
            "❌ Block {} does not extend locked block {}. Block does not satisfy safeNode predicate",
            block.id(),
            locked_block.id(),
        );
    }
    Ok(extends)
}
