use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use cadence_macros::statsd_time;
use dashmap::DashMap;
use indexmap::IndexMap;
use solana_client::rpc_client::RpcClient;
use solana_rpc_client_api::response::RpcContactInfo;
use solana_sdk::slot_history::Slot;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::errors::AtlasTxnSenderError;
use crate::solana_rpc::SolanaRpc;

pub trait LeaderTracker: Send + Sync {
    fn get_leaders(&self) -> Vec<RpcContactInfo>;
}

const NUM_LEADERS_PER_SLOT: usize = 4;

#[derive(Clone)]
pub struct LeaderTrackerImpl {
    rpc_client: Arc<RpcClient>,
    solana_rpc: Arc<dyn SolanaRpc>,
    cur_slot: Arc<AtomicU64>,
    cur_leaders: Arc<DashMap<Slot, RpcContactInfo>>,
    num_leaders: usize,
    leader_offset: i64,
}

impl LeaderTrackerImpl {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        solana_rpc: Arc<dyn SolanaRpc>,
        num_leaders: usize,
        leader_offset: i64,
    ) -> Self {
        let leader_tracker = Self {
            rpc_client,
            solana_rpc,
            cur_slot: Arc::new(AtomicU64::new(0)),
            cur_leaders: Arc::new(DashMap::new()),
            num_leaders,
            leader_offset,
        };
        leader_tracker.poll_slot();
        leader_tracker.poll_slot_leaders();
        leader_tracker
    }

    /// poll_slot polls for every new slot returned by gRPC geyser
    fn poll_slot(&self) {
        let solana_rpc = self.solana_rpc.clone();
        let cur_slot = self.cur_slot.clone();
        let leader_offset = self.leader_offset;
        tokio::spawn(async move {
            loop {
                let next_slot = solana_rpc.get_next_slot();
                let start_slot = next_slot.map(|s| _get_start_slot(s, leader_offset));
                if let Some(start_slot) = start_slot {
                    if start_slot > cur_slot.load(Ordering::Relaxed) {
                        cur_slot.store(start_slot, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    /// poll_slot_leaders polls every minute for the next 1000 slot leaders and populates the cur_leaders map with the slot and ContactInfo of each leader
    fn poll_slot_leaders(&self) {
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                let start = Instant::now();
                if let Err(e) = self_clone.poll_slot_leaders_once() {
                    error!("Error polling slot leaders: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                statsd_time!("poll_slot_leaders", start.elapsed());
                sleep(Duration::from_secs(60)).await;
            }
        });
    }

    fn poll_slot_leaders_once(&self) -> Result<(), AtlasTxnSenderError> {
        let next_slot = self.cur_slot.load(Ordering::Relaxed);
        debug!("Polling slot leaders for slot {}", next_slot);

        // polling 1000 slots ahead is more than enough
        let slot_leaders = match self.rpc_client.get_slot_leaders(next_slot, 1000) {
            Ok(leaders) => leaders,
            Err(e) => return Err(format!("Error getting slot leaders: {}", e).into()),
        };

        let new_cluster_nodes = match self.rpc_client.get_cluster_nodes() {
            Ok(nodes) => nodes,
            Err(e) => return Err(format!("Error getting cluster nodes: {}", e).into()),
        };

        let mut cluster_node_map = HashMap::new();
        for node in new_cluster_nodes {
            cluster_node_map.insert(node.pubkey.clone(), node);
        }

        for (i, leader) in slot_leaders.iter().enumerate() {
            let contact_info = cluster_node_map.get(&leader.to_string());
            if let Some(contact_info) = contact_info {
                self.cur_leaders
                    .insert(next_slot + i as u64, contact_info.clone());
                info!("Added leader: {} for slot: {}", leader, next_slot + i as u64);
            } else {
                warn!("Leader {} not found in cluster nodes", leader);
            }
        }

        self.clean_up_slot_leaders();
        Ok(())
    }

    fn clean_up_slot_leaders(&self) {
        let cur_slot = self.cur_slot.load(Ordering::Relaxed);
        let mut slots_to_remove = vec![];
        for leaders in self.cur_leaders.iter() {
            if leaders.key().clone() < cur_slot {
                slots_to_remove.push(leaders.key().clone());
            }
        }
        for slot in slots_to_remove {
            self.cur_leaders.remove(&slot);
        }
    }
}

fn _get_start_slot(next_slot: u64, leader_offset: i64) -> u64 {
    let slot_buffer = leader_offset * (NUM_LEADERS_PER_SLOT as i64);
    let start_slot = if slot_buffer > 0 {
        next_slot + slot_buffer as u64
    } else {
        next_slot - slot_buffer.abs() as u64
    };
    start_slot
}

impl LeaderTracker for LeaderTrackerImpl {
    fn get_leaders(&self) -> Vec<RpcContactInfo> {
        let start_slot = self.cur_slot.load(Ordering::Relaxed);
        let end_slot = start_slot + (self.num_leaders * NUM_LEADERS_PER_SLOT) as u64;
        let mut leaders = IndexMap::new();
        for slot in start_slot..end_slot {
            let leader = self.cur_leaders.get(&slot);
            if let Some(leader) = leader {
                _ = leaders.insert(leader.pubkey.to_owned(), leader.value().to_owned());
            }
            if leaders.len() >= self.num_leaders {
                break;
            }
        }
        
        let leader_list = leaders.values().cloned().collect::<Vec<_>>();
        info!(
            "Current leaders: {:?}, start_slot: {:?}",
            leader_list.iter().map(|l| l.pubkey.clone()).collect::<Vec<_>>(),
            start_slot
        );
        leader_list
    }
}

fn get_leader_tpu_address(&self, leader_pubkey: &str) -> Option<String> {
    // Add logging to check if the leader is found in the map
    if let Some(leader_info) = self.cur_leaders.get(leader_pubkey) {
        // Log the leader information
        debug!("Leader info found: {:?}", leader_info);

        // Check if TPU address is available
        if let Some(tpu_address) = leader_info.tpu {
            debug!("TPU address found: {}", tpu_address);
            return Some(tpu_address);
        } else {
            warn!("No TPU address found for leader: {}", leader_pubkey);
        }
    } else {
        warn!("Leader not found in the map: {}", leader_pubkey);
    }
    None
}
