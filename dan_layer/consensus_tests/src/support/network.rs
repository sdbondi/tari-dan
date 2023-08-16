//    Copyright 2023 The Tari Project
//    SPDX-License-Identifier: BSD-3-Clause

use std::{
    collections::HashMap,
    sync::{atomic::AtomicUsize, Arc},
};

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use itertools::Itertools;
use tari_consensus::messages::HotstuffMessage;
use tari_dan_common_types::{committee::Committee, shard_bucket::ShardBucket};
use tari_dan_storage::consensus_models::{Decision, ExecutedTransaction};
use tari_transaction::Transaction;
use tokio::sync::{
    mpsc::{self},
    watch,
};

use crate::support::{address::TestAddress, transaction::build_transaction_from, ValidatorChannels};

pub fn spawn_network(channels: Vec<ValidatorChannels>, default_decision: Decision, default_fee: u64) -> TestNetwork {
    let tx_new_transactions = channels
        .iter()
        .map(|c| (c.address.clone(), (c.bucket, c.tx_new_transactions.clone())))
        .collect();
    let tx_hs_message = channels
        .iter()
        .map(|c| (c.address.clone(), c.tx_hs_message.clone()))
        .collect();
    let (rx_broadcast, rx_leader, rx_mempool) = channels
        .into_iter()
        .map(|c| {
            (
                (c.address.clone(), c.rx_broadcast),
                (c.address.clone(), c.rx_leader),
                (c.address, c.rx_mempool),
            )
        })
        .multiunzip();
    let (tx_new_transaction, rx_new_transaction) = mpsc::channel(100);
    let (tx_network_status, network_status) = watch::channel(NetworkStatus::Paused);
    let (tx_on_message, rx_on_message) = watch::channel(None);
    let num_sent_messages = Arc::new(AtomicUsize::new(0));

    TestNetworkWorker {
        network_status,
        rx_new_transaction: Some(rx_new_transaction),
        tx_new_transactions,
        tx_hs_message,
        rx_broadcast: Some(rx_broadcast),
        rx_leader: Some(rx_leader),
        rx_mempool: Some(rx_mempool),
        on_message: tx_on_message,
        num_sent_messages: num_sent_messages.clone(),
        default_decision,
        default_fee,
    }
    .spawn();

    TestNetwork {
        tx_new_transaction,
        network_status: tx_network_status,
        num_sent_messages,
        _on_message: rx_on_message,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkStatus {
    Started,
    Paused,
}

impl NetworkStatus {
    pub fn is_paused(self) -> bool {
        matches!(self, NetworkStatus::Paused)
    }
}

pub struct TestNetwork {
    tx_new_transaction: mpsc::Sender<(TestNetworkDestination, ExecutedTransaction)>,
    network_status: watch::Sender<NetworkStatus>,
    num_sent_messages: Arc<AtomicUsize>,
    _on_message: watch::Receiver<Option<HotstuffMessage<TestAddress>>>,
}

impl TestNetwork {
    pub fn start(&self) {
        self.network_status.send(NetworkStatus::Started).unwrap();
    }

    #[allow(dead_code)]
    pub async fn on_message(&mut self) -> Option<HotstuffMessage<TestAddress>> {
        self._on_message.changed().await.unwrap();
        self._on_message.borrow().clone()
    }

    #[allow(dead_code)]
    pub async fn pause(&self) {
        self.network_status.send(NetworkStatus::Paused).unwrap();
    }

    pub async fn send_transaction(&self, destination: TestNetworkDestination, tx: ExecutedTransaction) {
        self.tx_new_transaction.send((destination, tx)).await.unwrap();
    }

    pub fn total_messages_sent(&self) -> usize {
        self.num_sent_messages.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub enum TestNetworkDestination {
    All,
    Address(TestAddress),
    Bucket(u32),
}

impl TestNetworkDestination {
    pub fn is_for(&self, addr: &TestAddress, bucket: ShardBucket) -> bool {
        match self {
            TestNetworkDestination::All => true,
            TestNetworkDestination::Address(a) => a == addr,
            TestNetworkDestination::Bucket(b) => *b == bucket,
        }
    }
}

pub struct TestNetworkWorker {
    rx_new_transaction: Option<mpsc::Receiver<(TestNetworkDestination, ExecutedTransaction)>>,
    tx_new_transactions: HashMap<TestAddress, (ShardBucket, mpsc::Sender<ExecutedTransaction>)>,
    tx_hs_message: HashMap<TestAddress, mpsc::Sender<(TestAddress, HotstuffMessage<TestAddress>)>>,
    #[allow(clippy::type_complexity)]
    rx_broadcast: Option<HashMap<TestAddress, mpsc::Receiver<(Committee<TestAddress>, HotstuffMessage<TestAddress>)>>>,
    #[allow(clippy::type_complexity)]
    rx_leader: Option<HashMap<TestAddress, mpsc::Receiver<(TestAddress, HotstuffMessage<TestAddress>)>>>,
    rx_mempool: Option<HashMap<TestAddress, mpsc::Receiver<Transaction>>>,
    network_status: watch::Receiver<NetworkStatus>,
    on_message: watch::Sender<Option<HotstuffMessage<TestAddress>>>,
    num_sent_messages: Arc<AtomicUsize>,
    default_decision: Decision,
    default_fee: u64,
}

impl TestNetworkWorker {
    pub fn spawn(self) {
        tokio::spawn(self.run());
    }

    async fn run(mut self) {
        let mut rx_broadcast = self.rx_broadcast.take().unwrap();
        let mut rx_leader = self.rx_leader.take().unwrap();
        let mut rx_mempool = self.rx_mempool.take().unwrap();

        let mut rx_new_transaction = self.rx_new_transaction.take().unwrap();
        let tx_new_transactions = self.tx_new_transactions.clone();

        tokio::spawn(async move {
            while let Some((dest, tx)) = rx_new_transaction.recv().await {
                for (addr, (bucket, tx_new_transaction)) in &tx_new_transactions {
                    if dest.is_for(addr, *bucket) {
                        tx_new_transaction.send(tx.clone()).await.unwrap();
                    }
                }
            }
        });

        if self.network_status.borrow().is_paused() {
            loop {
                self.network_status.changed().await.unwrap();
                if let NetworkStatus::Started = *self.network_status.borrow() {
                    break;
                }
            }
        }

        loop {
            let mut rx_broadcast = rx_broadcast
                .iter_mut()
                .map(|(from, rx)| rx.recv().map(|r| (from.clone(), r)))
                .collect::<FuturesUnordered<_>>();
            let mut rx_leader = rx_leader
                .iter_mut()
                .map(|(from, rx)| rx.recv().map(|r| (from.clone(), r)))
                .collect::<FuturesUnordered<_>>();

            let mut rx_mempool = rx_mempool
                .iter_mut()
                .map(|(from, rx)| rx.recv().map(|r| (from.clone(), r)))
                .collect::<FuturesUnordered<_>>();

            tokio::select! {
                Some((from, Some((to, msg)))) = rx_broadcast.next() => self.handle_broadcast(from, to, msg).await,
                Some((from, Some((to, msg)))) = rx_leader.next() => self.handle_leader(from, to, msg).await,
                Some((from, Some(msg))) = rx_mempool.next() => self.handle_mempool(from, msg).await,

                Ok(_) = self.network_status.changed() => {
                    if let NetworkStatus::Started = *self.network_status.borrow() {
                        continue;
                    }
                    loop{
                        self.network_status.changed().await.unwrap();
                        if let NetworkStatus::Started = *self.network_status.borrow() {
                            break;
                        }
                    }
                }
                else => break,
            }
        }
    }

    pub async fn handle_broadcast(
        &mut self,
        from: TestAddress,
        to: Committee<TestAddress>,
        msg: HotstuffMessage<TestAddress>,
    ) {
        self.num_sent_messages
            .fetch_add(to.len(), std::sync::atomic::Ordering::Relaxed);
        for vn in to {
            self.tx_hs_message
                .get(&vn)
                .unwrap()
                .send((from.clone(), msg.clone()))
                .await
                .unwrap();
        }
        self.on_message.send(Some(msg.clone())).unwrap();
    }

    pub async fn handle_leader(&mut self, from: TestAddress, to: TestAddress, msg: HotstuffMessage<TestAddress>) {
        self.on_message.send(Some(msg.clone())).unwrap();
        self.num_sent_messages
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tx_hs_message.get(&to).unwrap().send((from, msg)).await.unwrap();
    }

    pub async fn handle_mempool(&mut self, from: TestAddress, msg: Transaction) {
        let (_, sender) = self.tx_new_transactions.get(&from).unwrap();

        sender
            .send(build_transaction_from(msg, self.default_decision, self.default_fee))
            .await
            .unwrap();
    }
}
