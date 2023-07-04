//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashMap;

use futures::{stream::FuturesUnordered, StreamExt};
use tari_consensus::{hotstuff::HotstuffEvent, traits::EpochManager};
use tari_dan_common_types::{committee::Committee, NodeHeight};
use tari_dan_storage::{
    consensus_models::{Block, BlockId, Decision, TransactionPoolStage},
    StateStore,
    StateStoreReadTransaction,
};
use tokio::task;

use crate::support::{
    address::TestAddress,
    epoch_manager::TestEpochManager,
    network::{spawn_network, TestNetwork},
    transaction::build_transaction,
    validator::Validator,
    SelectedIndexLeaderStrategy,
    ValidatorChannels,
};

pub struct Test {
    validators: HashMap<TestAddress, Validator>,
    network: TestNetwork,
    leader_strategy: SelectedIndexLeaderStrategy,
    epoch_manager: TestEpochManager,
    messages_sent: HashMap<DanMessageType, usize>,
}

impl Test {
    pub fn builder() -> TestBuilder {
        TestBuilder::new()
    }

    pub fn total_messages_sent(&self) -> usize {
        self.messages_sent.values().sum()
    }

    pub async fn send_transaction_to_all(&self, decision: Decision, fee: u64, num_shards: usize) {
        let tx = build_transaction(decision, fee, num_shards);
        self.network.send_transaction(tx).await;
    }

    pub async fn on_hotstuff_event(&mut self) -> HotstuffEvent {
        self.validators
            .values_mut()
            .map(|v| v.events.recv())
            .collect::<FuturesUnordered<_>>()
            .next()
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn on_block_committed(&mut self) -> BlockId {
        // Remove when events are added
        #[allow(clippy::never_loop)]
        loop {
            let event = self.on_hotstuff_event().await;
            match event {
                HotstuffEvent::BlockCommitted { block_id } => return block_id,
                // _ => {}
            }
        }
    }

    pub fn network(&mut self) -> &mut TestNetwork {
        &mut self.network
    }

    pub fn get_validator_mut(&mut self, addr: &TestAddress) -> &mut Validator {
        self.validators.get_mut(addr).unwrap()
    }

    pub fn get_validator(&self, addr: &TestAddress) -> &Validator {
        self.validators.get(addr).unwrap()
    }

    pub fn are_all_transactions_committed(&self) -> bool {
        self.validators.values().all(|v| {
            let c = v.uncommitted_transaction_count();
            log::info!("{} has {} unfinalized transactions", v.address, c);
            c == 0
        })
    }

    pub fn with_all_validators(&self, f: impl FnMut(&Validator)) {
        self.validators.values().for_each(f);
    }

    pub async fn wait_until_new_pool_count(&self, count: usize) {
        self.wait_all_for_predicate(format!("new pool count to be {}", count), |v| {
            v.state_store
                .with_read_tx(|tx| tx.transaction_pool_count(None, None))
                .unwrap() >=
                count
        })
        .await;
    }

    pub async fn wait_for_all_to_have_leaf_height(&self, height: NodeHeight) {
        let epoch = self.epoch_manager.current_epoch().await.unwrap();
        self.wait_all_for_predicate(format!("leaf height to be {}", height), |v| {
            let leaf_height = v
                .state_store
                .with_read_tx(|tx| tx.leaf_block_get(epoch))
                .unwrap()
                .height;

            leaf_height >= height
        })
        .await;
    }

    pub async fn wait_all_have_at_least_n_new_transactions_in_pool(&self, n: usize) {
        self.wait_all_for_predicate(format!("waiting for {} new transaction(s) in pool", n), |v| {
            let pool_count = v
                .state_store
                .with_read_tx(|tx| tx.transaction_pool_count(Some(TransactionPoolStage::New), Some(true)))
                .unwrap();

            pool_count >= n
        })
        .await;
    }

    async fn wait_all_for_predicate<P: FnMut(&Validator) -> bool>(&self, description: String, mut predicate: P) {
        let mut complete = vec![];
        let mut remaining_loops = 100usize; // ~10 seconds
        loop {
            self.with_all_validators(|v| {
                if complete.contains(&v.address) {
                    return;
                }
                if predicate(v) {
                    complete.push(v.address);
                } else if remaining_loops == 0 {
                    panic!("Timed out waiting for {}", description);
                } else {
                    // 📎
                }
            });

            remaining_loops -= 1;

            if complete.len() == self.validators.len() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub async fn assert_all_validators_at_same_height(&self) {
        let epoch = self.epoch_manager.current_epoch().await.unwrap();
        let committees = self.epoch_manager.all_committees().await;
        let mut attempts = 0usize;
        'outer: loop {
            for committee in committees.values() {
                let mut heights = self
                    .validators
                    .values()
                    .filter(|vn| committee.members.contains(&vn.address))
                    .map(|v| {
                        let height = v
                            .state_store
                            .with_read_tx(|tx| Block::get_tip(tx, epoch))
                            .unwrap()
                            .height();
                        (v.address, height)
                    });
                let (first_addr, first) = heights.next().unwrap();
                for (addr, height) in heights {
                    if first != height && attempts < 5 {
                        attempts += 1;
                        // Send this task to the back of the queue and try again after other tasks have executed
                        // to allow validators to catch up
                        task::yield_now().await;
                        continue 'outer;
                    }
                    assert_eq!(
                        first, height,
                        "Validator {} is at height {} but validator {} is at height {}",
                        first_addr, first, addr, height
                    );
                }
            }
            break;
        }
    }

    pub async fn assert_clean_shutdown(mut self) {
        for v in self.validators.values_mut() {
            v.shutdown.trigger();
        }
        for v in self.validators.into_values() {
            v.handle.await.unwrap();
        }
    }
}

pub struct TestBuilder {
    committees: HashMap<u32, Committee<TestAddress>>,
    sql_address: String,
}

impl TestBuilder {
    pub fn new() -> Self {
        Self {
            committees: HashMap::new(),
            sql_address: ":memory:".to_string(),
        }
    }

    pub fn with_sql_url<T: Into<String>>(&mut self, sql_address: T) -> &mut Self {
        self.sql_address = sql_address.into();
        self
    }

    pub fn add_committee(&mut self, bucket: u32, addresses: Vec<&'static str>) -> &mut Self {
        self.committees
            .insert(bucket, addresses.into_iter().map(TestAddress).collect());
        self
    }

    async fn build_validators(
        &self,
        leader_strategy: &SelectedIndexLeaderStrategy,
        epoch_manager: &TestEpochManager,
    ) -> (Vec<ValidatorChannels>, HashMap<TestAddress, Validator>) {
        epoch_manager
            .all_validators()
            .await
            .into_iter()
            .map(|(address, shard)| {
                let leader_strategy = leader_strategy.clone();
                let sql_address = self.sql_address.replace("{}", address.0);
                let (channels, validator) = Validator::builder()
                    .with_sql_url(sql_address)
                    .with_address(address)
                    .with_shard(shard)
                    .with_epoch_manager(epoch_manager.clone_for(address, shard))
                    .with_leader_strategy(leader_strategy)
                    .spawn();
                (channels, (address, validator))
            })
            .unzip()
    }

    pub async fn start(&self) -> Test {
        let leader_strategy = SelectedIndexLeaderStrategy::new(0);
        let epoch_manager = TestEpochManager::new();
        epoch_manager.set_num_committees(self.committees.len() as u32).await;
        for (bucket, committee) in &self.committees {
            epoch_manager.add_committee(*bucket, committee.clone()).await;
        }
        let (channels, validators) = self.build_validators(&leader_strategy, &epoch_manager).await;
        let network = spawn_network(channels);

        Test {
            validators,
            network,
            leader_strategy,
            messages_sent: Default::default(),
            epoch_manager,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum DanMessageType {
    Proposal,
    Vote,
    NewView,
}
