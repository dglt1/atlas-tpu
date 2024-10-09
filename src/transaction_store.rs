use std::{sync::Arc, time::Instant};

use crate::rpc_server::RequestMetadata;
use cadence_macros::statsd_time;
use dashmap::DashMap;
use solana_sdk::transaction::VersionedTransaction;
use tracing::error;
use bincode; // Add this import

#[derive(Clone, Debug)]
pub struct TransactionData {
    pub wire_transaction: Vec<u8>,
    pub versioned_transaction: VersionedTransaction,
    pub sent_at: Instant,
    pub retry_count: usize,
    pub max_retries: usize,
    // might not be the best spot but is easy to add for what we need out of metrics now
    pub request_metadata: Option<RequestMetadata>,
}

impl TransactionData {
    pub fn from_versioned_transaction(transaction: VersionedTransaction) -> Self {
        // Serialize the transaction using bincode
        let wire_transaction = bincode::serialize(&transaction).expect("Failed to serialize transaction");

        TransactionData {
            wire_transaction,
            versioned_transaction: transaction,
            sent_at: Instant::now(),
            retry_count: 0,
            max_retries: 5, // Default or configurable value
            request_metadata: None, // Or populate if you have metadata
        }
    }
}

pub trait TransactionStore: Send + Sync {
    fn add_transaction(&self, transaction: TransactionData);
    fn get_signatures(&self) -> Vec<String>;
    fn remove_transaction(&self, signature: String) -> Option<TransactionData>;
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionData>>;
    fn has_signature(&self, signature: &str) -> bool;
}

pub struct TransactionStoreImpl {
    transactions: Arc<DashMap<String, TransactionData>>,
}

impl TransactionStoreImpl {
    pub fn new() -> Self {
        let transaction_store = Self {
            transactions: Arc::new(DashMap::new()),
        };
        transaction_store
    }
}

impl TransactionStore for TransactionStoreImpl {
    fn has_signature(&self, signature: &str) -> bool {
        self.transactions.contains_key(signature)
    }
    fn add_transaction(&self, transaction: TransactionData) {
        let start = Instant::now();
        if let Some(signature) = get_signature(&transaction) {
            if self.transactions.contains_key(&signature) {
                return;
            }
            self.transactions.insert(signature.to_string(), transaction);
        } else {
            error!("Transaction has no signatures");
        }
        statsd_time!("add_signature_time", start.elapsed());
    }
    fn get_signatures(&self) -> Vec<String> {
        let start = Instant::now();
        let signatures = self
            .transactions
            .iter()
            .map(|t| get_signature(&t).unwrap())
            .collect();
        statsd_time!("get_signatures_time", start.elapsed());
        signatures
    }
    fn remove_transaction(&self, signature: String) -> Option<TransactionData> {
        let start = Instant::now();
        let transaction = self.transactions.remove(&signature);
        statsd_time!("remove_signature_time", start.elapsed());
        transaction.map_or(None, |t| Some(t.1))
    }
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionData>> {
        self.transactions.clone()
    }
}

pub fn get_signature(transaction: &TransactionData) -> Option<String> {
    transaction
        .versioned_transaction
        .signatures
        .get(0)
        .map(|s| s.to_string())
}
