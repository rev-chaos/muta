extern crate test;

mod mempool;

use std::convert::{From, TryFrom};
use std::sync::Arc;

use async_trait::async_trait;
use chashmap::CHashMap;
use futures::executor;
use rand::random;
use rand::rngs::OsRng;
use rayon::iter::IntoParallelRefIterator;
use rayon::prelude::*;

use common_crypto::{
    Crypto, PrivateKey, PublicKey, Secp256k1, Secp256k1PrivateKey, Secp256k1PublicKey,
    Secp256k1Signature, Signature, ToPublicKey,
};
use protocol::codec::ProtocolCodec;
use protocol::traits::{Context, MemPool, MemPoolAdapter, MixedTxHashes};
use protocol::types::{Hash, RawTransaction, SignedTransaction, TransactionRequest};
use protocol::{Bytes, ProtocolResult};

use crate::{HashMemPool, MemPoolError};

const CYCLE_LIMIT: u64 = 10_000;
const CURRENT_HEIGHT: u64 = 999;
const POOL_SIZE: usize = 100_000;
const TIMEOUT: u64 = 1000;
const TIMEOUT_GAP: u64 = 100;
const TX_CYCLE: u64 = 1;

pub struct HashMemPoolAdapter {
    network_txs: CHashMap<Hash, SignedTransaction>,
}

impl HashMemPoolAdapter {
    fn new() -> HashMemPoolAdapter {
        HashMemPoolAdapter {
            network_txs: CHashMap::new(),
        }
    }
}

#[async_trait]
impl MemPoolAdapter for HashMemPoolAdapter {
    async fn pull_txs(
        &self,
        _ctx: Context,
        tx_hashes: Vec<Hash>,
    ) -> ProtocolResult<Vec<SignedTransaction>> {
        let mut vec = Vec::new();
        for hash in tx_hashes {
            if let Some(tx) = self.network_txs.get(&hash) {
                vec.push(tx.clone());
            }
        }
        Ok(vec)
    }

    async fn broadcast_tx(&self, _ctx: Context, tx: SignedTransaction) -> ProtocolResult<()> {
        self.network_txs.insert(tx.tx_hash.clone(), tx);
        Ok(())
    }

    async fn check_signature(&self, _ctx: Context, tx: SignedTransaction) -> ProtocolResult<()> {
        check_hash(tx.clone()).await?;
        check_sig(&tx)
    }

    async fn check_transaction(&self, _ctx: Context, _tx: SignedTransaction) -> ProtocolResult<()> {
        Ok(())
    }

    async fn check_storage_exist(&self, _ctx: Context, _tx_hash: Hash) -> ProtocolResult<()> {
        Ok(())
    }

    async fn get_latest_height(&self, _ctx: Context) -> ProtocolResult<u64> {
        Ok(CURRENT_HEIGHT)
    }

    fn set_timeout_gap(&self, _timeout_gap: u64) {}
}

pub fn default_mock_txs(size: usize) -> Vec<SignedTransaction> {
    mock_txs(size, 0, TIMEOUT)
}

fn mock_txs(valid_size: usize, invalid_size: usize, timeout: u64) -> Vec<SignedTransaction> {
    let mut vec = Vec::new();
    let priv_key = Secp256k1PrivateKey::generate(&mut OsRng);
    let pub_key = priv_key.pub_key();
    for i in 0..valid_size + invalid_size {
        vec.push(mock_signed_tx(&priv_key, &pub_key, timeout, i < valid_size));
    }
    vec
}

fn default_mempool() -> HashMemPool<HashMemPoolAdapter> {
    new_mempool(POOL_SIZE, TIMEOUT_GAP)
}

fn new_mempool(pool_size: usize, timeout_gap: u64) -> HashMemPool<HashMemPoolAdapter> {
    let adapter = HashMemPoolAdapter::new();
    let mempool = HashMemPool::new(pool_size, adapter);
    mempool.set_timeout_gap(timeout_gap);
    mempool
}

async fn check_hash(tx: SignedTransaction) -> ProtocolResult<()> {
    let mut raw = tx.raw;
    let raw_bytes = raw.encode().await?;
    let tx_hash = Hash::digest(raw_bytes);
    if tx_hash != tx.tx_hash {
        return Err(MemPoolError::CheckHash {
            expect: tx.tx_hash.clone(),
            actual: tx_hash,
        }
        .into());
    }
    Ok(())
}

fn check_sig(tx: &SignedTransaction) -> ProtocolResult<()> {
    if Secp256k1::verify_signature(&tx.tx_hash.as_bytes(), &tx.signature, &tx.pubkey).is_err() {
        return Err(MemPoolError::CheckSig {
            tx_hash: tx.tx_hash.clone(),
        }
        .into());
    }
    Ok(())
}

fn concurrent_check_sig(txs: Vec<SignedTransaction>) {
    txs.par_iter().for_each(|signed_tx| {
        check_sig(signed_tx).unwrap();
    });
}

fn concurrent_insert(txs: Vec<SignedTransaction>, mempool: Arc<HashMemPool<HashMemPoolAdapter>>) {
    txs.par_iter()
        .for_each(|signed_tx| exec_insert(signed_tx, Arc::clone(&mempool)));
}

fn concurrent_broadcast(
    txs: Vec<SignedTransaction>,
    mempool: Arc<HashMemPool<HashMemPoolAdapter>>,
) {
    txs.par_iter().for_each(|signed_tx| {
        executor::block_on(async {
            mempool
                .get_adapter()
                .broadcast_tx(Context::new(), signed_tx.clone())
                .await
                .unwrap();
        })
    });
}

fn exec_insert(signed_tx: &SignedTransaction, mempool: Arc<HashMemPool<HashMemPoolAdapter>>) {
    executor::block_on(async {
        let _ = mempool.insert(Context::new(), signed_tx.clone()).await;
    });
}

fn exec_flush(remove_hashes: Vec<Hash>, mempool: Arc<HashMemPool<HashMemPoolAdapter>>) {
    executor::block_on(async {
        mempool.flush(Context::new(), remove_hashes).await.unwrap();
    });
}

fn exec_package(mempool: Arc<HashMemPool<HashMemPoolAdapter>>, cycle_limit: u64) -> MixedTxHashes {
    executor::block_on(async { mempool.package(Context::new(), cycle_limit).await.unwrap() })
}

fn exec_ensure_order_txs(require_hashes: Vec<Hash>, mempool: Arc<HashMemPool<HashMemPoolAdapter>>) {
    executor::block_on(async {
        mempool
            .ensure_order_txs(Context::new(), require_hashes)
            .await
            .unwrap();
    })
}

fn exec_sync_propose_txs(require_hashes: Vec<Hash>, mempool: Arc<HashMemPool<HashMemPoolAdapter>>) {
    executor::block_on(async {
        mempool
            .sync_propose_txs(Context::new(), require_hashes)
            .await
            .unwrap();
    })
}

fn exec_get_full_txs(
    require_hashes: Vec<Hash>,
    mempool: Arc<HashMemPool<HashMemPoolAdapter>>,
) -> Vec<SignedTransaction> {
    executor::block_on(async {
        mempool
            .get_full_txs(Context::new(), require_hashes)
            .await
            .unwrap()
    })
}

fn mock_signed_tx(
    priv_key: &Secp256k1PrivateKey,
    pub_key: &Secp256k1PublicKey,
    timeout: u64,
    valid: bool,
) -> SignedTransaction {
    let nonce = Hash::digest(Bytes::from(get_random_bytes(10)));

    let request = TransactionRequest {
        service_name: "test".to_owned(),
        method:       "test".to_owned(),
        payload:      "test".to_owned(),
    };
    let mut raw = RawTransaction {
        chain_id: nonce.clone(),
        nonce,
        timeout,
        cycles_limit: TX_CYCLE,
        cycles_price: 1,
        request,
    };

    let raw_bytes = executor::block_on(async { raw.encode().await.unwrap() });
    let tx_hash = Hash::digest(raw_bytes);

    let signature = if valid {
        Secp256k1::sign_message(&tx_hash.as_bytes(), &priv_key.to_bytes()).unwrap()
    } else {
        Secp256k1Signature::try_from([0u8; 64].as_parallel_slice()).unwrap()
    };

    SignedTransaction {
        raw,
        tx_hash,
        pubkey: pub_key.to_bytes(),
        signature: signature.to_bytes(),
    }
}

fn get_random_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|_| random::<u8>()).collect()
}

fn check_order_consistant(mixed_tx_hashes: &MixedTxHashes, txs: &[SignedTransaction]) -> bool {
    mixed_tx_hashes
        .order_tx_hashes
        .iter()
        .enumerate()
        .any(|(i, hash)| hash == &txs.get(i).unwrap().tx_hash)
}
