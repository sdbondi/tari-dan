//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::collections::HashSet;

use log::*;
use tari_dan_common_types::{shard_bucket::ShardBucket, Epoch, PeerAddress, ShardId};
use tari_dan_p2p::{proto, DanMessage};
use tari_epoch_manager::{base_layer::EpochManagerHandle, EpochManagerReader};

use crate::p2p::services::{mempool::MempoolError, messaging::Gossip};

const LOG_TARGET: &str = "tari::validator_node::mempool::gossip";

#[derive(Debug)]
pub(super) struct MempoolGossip<TAddr> {
    epoch_manager: EpochManagerHandle<TAddr>,
    gossip: Gossip,
    is_subscribed: Option<ShardBucket>,
}

impl MempoolGossip<PeerAddress> {
    pub fn new(epoch_manager: EpochManagerHandle<PeerAddress>, outbound: Gossip) -> Self {
        Self {
            epoch_manager,
            gossip: outbound,
            is_subscribed: None,
        }
    }

    pub async fn next_message(&mut self) -> Option<Result<(PeerAddress, DanMessage), MempoolError>> {
        self.gossip
            .next_message()
            .await
            .map(|result| result.map_err(MempoolError::InvalidMessage))
    }

    pub async fn subscribe(&mut self, epoch: Epoch) -> Result<(), MempoolError> {
        let committee_shard = self.epoch_manager.get_local_committee_shard(epoch).await?;
        match self.is_subscribed {
            Some(b) if b == committee_shard.bucket() => {
                return Ok(());
            },
            Some(_) => {
                self.unsubscribe().await?;
            },
            None => {},
        }

        self.gossip
            .subscribe_topic(format!("transactions-{}", committee_shard.bucket()))
            .await?;
        self.is_subscribed = Some(committee_shard.bucket());
        Ok(())
    }

    pub async fn unsubscribe(&mut self) -> Result<(), MempoolError> {
        if let Some(b) = self.is_subscribed {
            self.gossip.unsubscribe_topic(format!("transactions-{}", b)).await?;
            self.is_subscribed = None;
        }
        Ok(())
    }

    pub async fn forward_to_local_replicas(&mut self, epoch: Epoch, msg: DanMessage) -> Result<(), MempoolError> {
        let committee = self.epoch_manager.get_local_committee_shard(epoch).await?;

        let topic = format!("transactions-{}", committee.bucket());
        debug!(
            target: LOG_TARGET,
            "forward_to_local_replicas: topic: {}", topic,
        );

        let msg = proto::network::DanMessage::from(&msg);
        self.gossip.publish_message(topic, msg).await?;

        Ok(())
    }

    pub async fn forward_to_foreign_replicas<T: Into<DanMessage>>(
        &mut self,
        epoch: Epoch,
        shards: HashSet<ShardId>,
        msg: T,
        exclude_bucket: Option<ShardBucket>,
    ) -> Result<(), MempoolError> {
        let n = self.epoch_manager.get_num_committees(epoch).await?;
        let local_shard = self.epoch_manager.get_local_committee_shard(epoch).await?;
        let local_bucket = local_shard.bucket();
        let buckets = shards
            .into_iter()
            .map(|s| s.to_committee_bucket(n))
            .filter(|b| exclude_bucket.as_ref() != Some(b) && b != &local_bucket)
            .collect::<HashSet<_>>();

        let msg = proto::network::DanMessage::from(&msg.into());
        for bucket in buckets {
            let topic = format!("transactions-{}", bucket);
            debug!(
                target: LOG_TARGET,
                "forward_to_foreign_replicas: topic: {}", topic,
            );

            self.gossip.publish_message(topic, msg.clone()).await?;
        }

        // let committees = self.epoch_manager.get_committees_by_shards(epoch, shards).await?;
        // let local_shard = self.epoch_manager.get_local_committee_shard(epoch).await?;
        // let local_committee = self.epoch_manager.get_local_committee(epoch).await?;
        //
        // if local_committee.is_empty() {
        //     error!(target: LOG_TARGET, "BUG: forward_to_foreign_replicas: get_local_committee returned empty
        // committee");     return Ok(());
        // }
        //
        // let Some(our_index) = local_committee
        //     .members()
        //     .position(|addr| addr == &self.validator_address)
        // else {
        //     error!(target: LOG_TARGET, "BUG: forward_to_foreign_replicas: get_local_committee returned committee that
        // this node is not part of");     return Ok(());
        // };
        //
        // let mut selected_members = vec![];
        // for (bucket, committee) in committees {
        //     // Dont forward locally
        //     if bucket == local_shard.bucket() {
        //         continue;
        //     }
        //     if exclude_bucket.map(|b| b == bucket).unwrap_or(false) {
        //         continue;
        //     }
        //     if committee.is_empty() {
        //         error!(
        //             target: LOG_TARGET,
        //             "BUG: forward_to_foreign_replicas: get_committees_by_shards returned empty committee"
        //         );
        //         continue;
        //     }
        //     let n = if local_committee.len() > committee.len() {
        //         // Our local committee is bigger, so we send to a single node
        //         1
        //     } else {
        //         // Our local committee is smaller, so we send to a portion of their nodes
        //         committee.len() / local_committee.len()
        //     };
        //
        //     selected_members.extend(committee.select_n_starting_from(n, our_index).cloned());
        // }
        //
        // debug!(
        //     target: LOG_TARGET,
        //     "forward_to_foreign_replicas: {} member(s) selected",
        //     selected_members.len(),
        // );
        //
        // if selected_members.is_empty() {
        //     return Ok(());
        // }
        //
        // // TODO: change this to use goissipsub
        // self.outbound.broadcast(selected_members.iter(), msg).await?;

        Ok(())
    }

    pub async fn gossip_to_foreign_replicas<T: Into<DanMessage>>(
        &mut self,
        epoch: Epoch,
        shards: HashSet<ShardId>,
        msg: T,
    ) -> Result<(), MempoolError> {
        // let committees = self.epoch_manager.get_committees_by_shards(epoch, shards).await?;
        // let local_shard = self.epoch_manager.get_local_committee_shard(epoch).await?;
        //
        // let mut selected_members = vec![];
        // for (bucket, committee) in committees {
        //     // Dont forward locally
        //     if bucket == local_shard.bucket() {
        //         continue;
        //     }
        //     if committee.is_empty() {
        //         error!(
        //             target: LOG_TARGET,
        //             "BUG: gossip_to_foreign_replicas: get_committees_by_shards returned empty committee"
        //         );
        //         continue;
        //     }
        //     let f = committee.max_failures();
        //
        //     selected_members.extend(committee.select_n_random(f + 1).cloned());
        // }
        //
        // debug!(
        //     target: LOG_TARGET,
        //     "gossip_to_foreign_replicas: {} member(s) selected",
        //     selected_members.len(),
        // );
        //
        // if selected_members.is_empty() {
        //     return Ok(());
        // }
        //
        // self.outbound.broadcast(selected_members.iter(), msg).await?;

        self.forward_to_foreign_replicas(epoch, shards, msg, None).await?;

        Ok(())
    }
}
