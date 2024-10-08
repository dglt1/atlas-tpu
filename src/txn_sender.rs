use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
    rpc_client::RpcClient,
};
use solana_program_runtime::compute_budget::{ComputeBudget, MAX_COMPUTE_UNIT_LIMIT};
use solana_sdk::transaction::{self, VersionedTransaction};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    runtime::{Builder, Runtime},
    time::{error::Elapsed, sleep, timeout},
};
use tonic::async_trait;
use tracing::{error, info, warn};
use dashmap::DashMap;
use tracing_subscriber::{EnvFilter, fmt::Subscriber};
use tracing_subscriber::util::SubscriberInitExt;
use std::sync::Once;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::time;

use crate::{
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
};
use solana_program_runtime::compute_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
use solana_sdk::borsh0_10::try_from_slice_unchecked;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use std::env;

const RETRY_COUNT_BINS: [i32; 6] = [0, 1, 2, 5, 10, 25];
const MAX_RETRIES_BINS: [i32; 5] = [0, 1, 5, 10, 30];
const MAX_TIMEOUT_SEND_DATA: Duration = Duration::from_millis(500);
const MAX_TIMEOUT_SEND_DATA_BATCH: Duration = Duration::from_millis(500);
const SEND_TXN_RETRIES: usize = 10;

// Add this at the top of your file, outside of any function
static TRACING_INIT: Once = Once::new();

// At the top of your file or in a configuration module
fn get_min_transaction_fee() -> u64 {
    env::var("MIN_TRANSACTION_FEE")
        .unwrap_or_else(|_| "30000".to_string()) // Default to 30000 if not set
        .parse()
        .expect("MIN_TRANSACTION_FEE must be a valid integer")
}

fn get_txn_send_retry_interval() -> Duration {
    let ms = env::var("TXN_SEND_RETRY_INTERVAL")
        .unwrap_or_else(|_| "1000".to_string())  // Default to 1000ms if not set
        .parse::<u64>()
        .expect("TXN_SEND_RETRY_INTERVAL must be a valid integer");
    
    Duration::from_millis(ms)
}

struct ValidatorInfo {
    pubkey: Pubkey,
    tpu_address: String,
    last_updated: Instant,
}

#[async_trait]
pub trait TxnSender: Send + Sync {
    fn send_transaction(&self, txn: TransactionData);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
    solana_rpc: Arc<dyn SolanaRpc>,
    txn_sender_runtime: Arc<Runtime>,
    txn_send_retry_interval_seconds: usize,
    max_retry_queue_size: Option<usize>,
    validator_info: Arc<Mutex<Vec<ValidatorInfo>>>,
    rpc_client: Arc<RpcClient>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
        solana_rpc: Arc<dyn SolanaRpc>,
        txn_sender_threads: usize,
        txn_send_retry_interval_seconds: usize,
        max_retry_queue_size: Option<usize>,
    ) -> Self {
        // Initialize tracing subscriber only once
        TRACING_INIT.call_once(|| {
            let env_filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info"));

            let subscriber = Subscriber::builder()
                .with_env_filter(env_filter)
                .finish();

            // Use set_global_default instead of try_init
            if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
                eprintln!("Warning: Failed to set global default subscriber: {}", e);
            }
        });

        let txn_sender_runtime = Builder::new_multi_thread()
            .worker_threads(txn_sender_threads)
            .enable_all()
            .build()
            .unwrap();

        let validator_pubkeys = env::var("VALIDATOR_PUBKEYS")
            .expect("VALIDATOR_PUBKEYS must be set")
            .split(',')
            .map(|s| Pubkey::from_str(s).expect("Invalid pubkey"))
            .collect::<Vec<_>>();

        let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "http://localhost:8899".to_string());
        let rpc_client = Arc::new(RpcClient::new(rpc_url));

        let validator_info = Arc::new(Mutex::new(Vec::new()));

        let sender = Self {
            leader_tracker,
            transaction_store,
            connection_cache,
            solana_rpc,
            txn_sender_runtime: Arc::new(txn_sender_runtime),
            txn_send_retry_interval_seconds,
            max_retry_queue_size,
            validator_info: validator_info.clone(),
            rpc_client: rpc_client.clone(),
        };

        // Update validator info immediately
        sender.update_validator_info(&validator_pubkeys);

        // Start a background task to update validator info periodically
        let validator_pubkeys = validator_pubkeys.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(600)); // 10 minutes

            loop {
                // Wait for the next interval tick
                interval.tick().await;

                // Update validator info
                Self::update_validator_info_task(validator_info.clone(), rpc_client.clone(), &validator_pubkeys).await;
            }
        });

        sender.retry_transactions();
        sender
    }

    fn retry_transactions(&self) {
        let leader_tracker = self.leader_tracker.clone();
        let transaction_store = self.transaction_store.clone();
        let connection_cache = self.connection_cache.clone();
        let txn_sender_runtime = self.txn_sender_runtime.clone();
        let txn_send_retry_interval_seconds = self.txn_send_retry_interval_seconds.clone();
        let max_retry_queue_size = self.max_retry_queue_size.clone();
        tokio::spawn(async move {
            loop {
                let mut transactions_reached_max_retries = vec![];
                let transaction_map = transaction_store.get_transactions();
                let queue_length = transaction_map.len();
                statsd_gauge!("transaction_retry_queue_length", queue_length as u64);

                // Log each transaction's data using tracing's info macro
                for entry in transaction_map.iter() {
                    let signature = entry.key();
                    let transaction_data = entry.value();
                    info!("Transaction Signature: {}, Data: {:?}", signature, transaction_data);
                }

                // Shed transactions by retry_count, if necessary.
                if let Some(max_size) = max_retry_queue_size {
                    if queue_length > max_size {
                        warn!(
                            "Transaction retry queue length is over the limit of {}: {}. Load shedding transactions with highest retry count.", 
                            max_size,
                            queue_length
                        );
                        let mut transactions: Vec<(String, TransactionData)> = transaction_map
                            .iter()
                            .map(|x| (x.key().to_owned(), x.value().to_owned()))
                            .collect();
                        transactions.sort_by(|(_, a), (_, b)| a.retry_count.cmp(&b.retry_count));
                        let transactions_to_remove = transactions[(max_size + 1)..].to_vec();
                        for (signature, _) in transactions_to_remove {
                            transaction_store.remove_transaction(signature.clone());
                            transaction_map.remove(&signature);
                        }
                        let records_dropped = queue_length - max_size;
                        statsd_gauge!("transactions_retry_queue_dropped", records_dropped as u64);
                    }
                }

                let mut wire_transactions = vec![];
                for mut transaction_data in transaction_map.iter_mut() {
                    wire_transactions.push(transaction_data.wire_transaction.clone());
                    if transaction_data.retry_count >= transaction_data.max_retries {
                        transactions_reached_max_retries
                            .push(get_signature(&transaction_data).unwrap());
                    } else {
                        transaction_data.retry_count += 1;
                    }
                }
                for wire_transaction in wire_transactions.iter() {
                    let mut leader_num = 0;
                    for leader in leader_tracker.get_leaders() {
                        if leader.tpu_quic.is_none() {
                            error!("leader {:?} has no tpu_quic", leader);
                            continue;
                        }
                        let connection_cache = connection_cache.clone();
                        let sent_at = Instant::now();
                        let leader = Arc::new(leader.clone());
                        let wire_transaction = wire_transaction.clone();
                        txn_sender_runtime.spawn(async move {
                        // retry unless its a timeout
                        for i in 0..SEND_TXN_RETRIES {
                            let conn = connection_cache
                                .get_nonblocking_connection(&leader.tpu_quic.unwrap());
                            if let Ok(result) = timeout(MAX_TIMEOUT_SEND_DATA_BATCH, conn.send_data(&wire_transaction)).await {
                                if let Err(e) = result {
                                    if i == SEND_TXN_RETRIES-1 {
                                        error!(
                                            retry = "true",
                                            "Failed to send transaction batch to {:?}: {}",
                                            leader, e
                                        );
                                        statsd_count!("transaction_send_error", 1, "retry" => "true", "last_attempt" => "true");
                                    } else {
                                        statsd_count!("transaction_send_error", 1, "retry" => "true", "last_attempt" => "false");
                                    }
                                } else {
                                    let leader_num_str = leader_num.to_string();
                                    statsd_time!(
                                        "transaction_received_by_leader",
                                        sent_at.elapsed(), "leader_num" => &leader_num_str, "api_key" => "not_applicable", "retry" => "true");
                                    return;
                                }
                            } else {
                                // Note: This is far too frequent to log. It will fill the disks on the host and cost too much on DD.
                                statsd_count!("transaction_send_timeout", 1);
                            }
                        }
                    });
                        leader_num += 1;
                    }
                }
                // remove transactions that reached max retries
                for signature in transactions_reached_max_retries {
                    let _ = transaction_store.remove_transaction(signature);
                    statsd_count!("transactions_reached_max_retries", 1);
                }
                sleep(get_txn_send_retry_interval()).await;
            }
        });
    }

    fn track_transaction(&self, transaction_data: &TransactionData) {
        let sent_at = transaction_data.sent_at.clone();
        let signature = get_signature(transaction_data);
        if signature.is_none() {
            return;
        }
        let signature = signature.unwrap();
        self.transaction_store
            .add_transaction(transaction_data.clone());
        let PriorityDetails {
            fee,
            cu_limit,
            priority,
        } = compute_priority_details(&transaction_data.versioned_transaction);
        let priority_fees_enabled = (fee > 0).to_string();
        let solana_rpc = self.solana_rpc.clone();
        let transaction_store = self.transaction_store.clone();
        let api_key = transaction_data
            .request_metadata
            .clone()
            .map(|m| m.api_key.clone())
            .unwrap_or("none".to_string());
        self.txn_sender_runtime.spawn(async move {
            let confirmed_at = solana_rpc.confirm_transaction(signature.clone()).await;
            let transcation_data = transaction_store.remove_transaction(signature);
            let mut retries = None;
            let mut max_retries = None;
            if let Some(transaction_data) = transcation_data {
                retries = Some(transaction_data.retry_count as i32);
                max_retries = Some(transaction_data.max_retries as i32);
            }

            let retries_tag = bin_counter_to_tag(retries, &RETRY_COUNT_BINS.to_vec());
            let max_retries_tag: String = bin_counter_to_tag(max_retries, &MAX_RETRIES_BINS.to_vec());

            // Collect metrics
            // We separate the retry metrics to reduce the cardinality with API key and price.
            let landed = if let Some(confirmed_at) = confirmed_at {
                statsd_count!("transactions_landed", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                statsd_count!("transactions_landed_by_key", 1, "api_key" => &api_key);
                statsd_time!("transaction_land_time", sent_at.elapsed(), "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                "true"
            } else {
                statsd_count!("transactions_not_landed", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                statsd_count!("transactions_not_landed_by_key", 1, "api_key" => &api_key);
                statsd_count!("transactions_not_landed_retries", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                "false"
            };
            statsd_time!("transaction_priority", priority, "landed" => &landed);
            statsd_time!("transaction_priority_fee", fee, "landed" => &landed);
            statsd_time!("transaction_compute_limit", cu_limit as u64, "landed" => &landed);
        });
    }

    fn send_to_tpu_peers(&self, wire_transaction: Vec<u8>) {
        // List of TPU peers
        let tpu_peers = self.get_tpu_addresses();

        for peer in tpu_peers {
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = wire_transaction.clone();
            self.txn_sender_runtime.spawn(async move {
                if let Ok(socket_addr) = peer.parse::<std::net::SocketAddr>() {
                    for i in 0..SEND_TXN_RETRIES {
                        // Log the attempt to send the transaction
                        info!("Attempting to send transaction to peer: {}", peer);

                        let conn = connection_cache.get_nonblocking_connection(&socket_addr);
                        if let Ok(result) = timeout(MAX_TIMEOUT_SEND_DATA, conn.send_data(&wire_transaction)).await {
                            if let Err(e) = result {
                                if i == SEND_TXN_RETRIES - 1 {
                                    error!(
                                        retry = "false",
                                        "Failed to send transaction to {:?}: {}",
                                        peer, e
                                    );
                                    statsd_count!("transaction_send_error", 1, "retry" => "false", "last_attempt" => "true");
                                } else {
                                    statsd_count!("transaction_send_error", 1, "retry" => "false", "last_attempt" => "false");
                                }
                            } else {
                                info!("Successfully sent transaction to peer: {}", peer);
                                statsd_time!(
                                    "transaction_received_by_peer",
                                    Instant::now().elapsed(), "peer" => &peer, "retry" => "false");
                                return;
                            }
                        } else {
                            statsd_count!("transaction_send_timeout", 1);
                        }
                    }
                } else {
                    error!("Invalid socket address: {}", peer);
                }
            });
        }
    }

    fn send_transaction(&self, transaction_data: TransactionData) {
        let signature = transaction_data.versioned_transaction.signatures[0].to_string();
        let priority_details = compute_priority_details(&transaction_data.versioned_transaction);
        let min_fee = get_min_transaction_fee();

        if priority_details.fee >= min_fee {
            info!("Transaction accepted: Signature: {}, Fee: {} lamports", signature, priority_details.fee);
            self.track_transaction(&transaction_data);
            self.send_to_tpu_peers(transaction_data.wire_transaction.clone());
        } else {
            warn!(
                "Transaction dropped: Signature: {}, Insufficient fee. Required: {} lamports, Actual: {} lamports",
                signature, min_fee, priority_details.fee
            );
            statsd_count!("transactions_dropped_insufficient_fee", 1);
        }
    }

    fn update_validator_info(&self, validator_pubkeys: &[Pubkey]) {
        info!("Updating validator info for {} pubkeys...", validator_pubkeys.len());
        let cluster_nodes = match self.rpc_client.get_cluster_nodes() {
            Ok(nodes) => nodes,
            Err(e) => {
                error!("Failed to get cluster nodes: {}", e);
                return;
            }
        };

        let mut updated_info = Vec::new();

        for node in cluster_nodes {
            if let Ok(pubkey) = Pubkey::from_str(&node.pubkey) {
                if validator_pubkeys.contains(&pubkey) {
                    let tpu_address = node.tpu.clone().unwrap_or_default();
                    updated_info.push(ValidatorInfo {
                        pubkey,
                        tpu_address: tpu_address.clone(),
                        last_updated: Instant::now(),
                    });
                    info!("Updated info for validator {}: TPU address {}", pubkey, tpu_address);
                }
            }
        }

        let mut validator_info = self.validator_info.lock().unwrap();
        *validator_info = updated_info;
        info!("Validator info update complete. Updated {} validators.", updated_info.len());

        if updated_info.len() < validator_pubkeys.len() {
            warn!("Some validators were not found in the cluster nodes. Expected {}, found {}.", 
                  validator_pubkeys.len(), updated_info.len());
        }
    }

    async fn update_validator_info_task(
        validator_info: Arc<Mutex<Vec<ValidatorInfo>>>,
        rpc_client: Arc<RpcClient>,
        validator_pubkeys: &[Pubkey],
    ) {
        info!("Updating validator info in background task...");
        let cluster_nodes = match rpc_client.get_cluster_nodes() {
            Ok(nodes) => nodes,
            Err(e) => {
                error!("Failed to get cluster nodes in background task: {}", e);
                return;
            }
        };

        let mut updated_info = Vec::new();

        for node in cluster_nodes {
            if let Ok(pubkey) = Pubkey::from_str(&node.pubkey) {
                if validator_pubkeys.contains(&pubkey) {
                    updated_info.push(ValidatorInfo {
                        pubkey,
                        tpu_address: node.tpu.unwrap_or_default(),
                        last_updated: Instant::now(),
                    });
                }
            }
        }

        let mut validator_info = validator_info.lock().unwrap();
        *validator_info = updated_info;
        info!("Validator info updated successfully in background task");
    }

    fn get_tpu_addresses(&self) -> Vec<String> {
        let validator_info = self.validator_info.lock().unwrap();
        validator_info.iter().map(|info| info.tpu_address.clone()).collect()
    }
}

pub struct PriorityDetails {
    pub fee: u64,
    pub cu_limit: u32,
    pub priority: u64,
}

pub fn compute_priority_details(transaction: &VersionedTransaction) -> PriorityDetails {
    let mut cu_limit = DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
    let mut compute_budget = ComputeBudget::new(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64);
    if let Err(e) = transaction.sanitize() {
        return PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        };
    }
    let instructions = transaction.message.instructions().iter().map(|ix| {
        match try_from_slice_unchecked(&ix.data) {
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                cu_limit = compute_unit_limit.min(MAX_COMPUTE_UNIT_LIMIT);
            }
            _ => {}
        }
        (
            transaction
                .message
                .static_account_keys()
                .get(usize::from(ix.program_id_index))
                .expect("program id index is sanitized"),
            ix,
        )
    });
    let compute_budget = compute_budget.process_instructions(instructions, true, true);
    match compute_budget {
        Ok(compute_budget) => PriorityDetails {
            fee: compute_budget.get_fee(),
            priority: compute_budget.get_priority(),
            cu_limit,
        },
        Err(e) => PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        },
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    fn send_transaction(&self, transaction_data: TransactionData) {
        self.send_transaction(transaction_data);
    }
}

fn bin_counter_to_tag(counter: Option<i32>, bins: &Vec<i32>) -> String {
    if counter.is_none() {
        return "none".to_string();
    }
    let counter = counter.unwrap();

    // Iterate through the bins vector to find the appropriate bin
    let mut bin_start = "-inf".to_string();
    let mut bin_end = "inf".to_string();
    for bin in bins.iter().rev() {
        if counter >= *bin {
            bin_start = bin.to_string();
            break;
        }

        bin_end = bin.to_string();
    }
    format!("{}_{}", bin_start, bin_end)
}

#[test]
fn test_bin_counter() {
    let bins = vec![0, 1, 2, 5, 10, 25];
    assert_eq!(bin_counter_to_tag(None, &bins), "none");
    assert_eq!(bin_counter_to_tag(Some(-100), &bins), "-inf_0");
    assert_eq!(bin_counter_to_tag(Some(0), &bins), "0_1");
    assert_eq!(bin_counter_to_tag(Some(1), &bins), "1_2");
    assert_eq!(bin_counter_to_tag(Some(2), &bins), "2_5");
    assert_eq!(bin_counter_to_tag(Some(3), &bins), "2_5");
    assert_eq!(bin_counter_to_tag(Some(17), &bins), "10_25");
    assert_eq!(bin_counter_to_tag(Some(34), &bins), "25_inf");
}