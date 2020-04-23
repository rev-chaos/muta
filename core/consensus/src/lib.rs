use std::boxed::Box;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use derive_more::Display;
use futures::channel::mpsc::UnboundedSender;
use overlord::types::{Aggregates, ExecResult, HeightRange, TinyHex, Vote, SelectMode, Node, AuthConfig};
use overlord::{
    Adapter, Address, Blk, BlockState, DefaultCrypto, Hash, Height, OverlordError, OverlordMsg,
    Proof, St, TimeConfig, OverlordConfig,
};
use parking_lot::RwLock;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use common_merkle::Merkle;

use protocol::traits::{
    CommonConsensusAdapter, ConsensusAdapter, Context, ExecutorFactory, ExecutorParams,
    ExecutorResp, Gossip, MemPool, MessageTarget, MixedTxHashes, Priority, Rpc, ServiceMapping,
    Storage, SynchronizationAdapter,
};
use protocol::types::{
    Address as ProtoAddress, Block, BlockHeader, Bloom, Bytes, FullBlock, Hash as ProtoHash,
    MerkleRoot, Metadata, Pill, Proof as ProtoProof, Receipt, SignedTransaction,
    TransactionRequest, Validator, ValidatorExtend
};
use protocol::{fixed_codec::FixedCodec, ProtocolError, ProtocolErrorKind, ProtocolResult};

struct Status {
    chain_id:              ProtoHash,
    address:               ProtoAddress,

    from_myself: RwLock<HashSet<Bytes>>,
}

pub struct OverlordAdapter<
    EF: ExecutorFactory<DB, S, Mapping>,
    G: Gossip,
    M: MemPool,
    R: Rpc,
    S: Storage,
    DB: cita_trie::DB,
    Mapping: ServiceMapping,
> {
    status:          Status,
    rpc:             Arc<R>,
    network:         Arc<G>,
    mem_pool:        Arc<M>,
    storage:         Arc<S>,
    trie_db:         Arc<DB>,
    service_mapping: Arc<Mapping>,

    phantom: PhantomData<EF>,
}

#[async_trait]
impl<EF, G, M, R, S, DB, Mapping> Adapter<WrappedPill, ExecResp>
    for OverlordAdapter<EF, G, M, R, S, DB, Mapping>
where
    EF: ExecutorFactory<DB, S, Mapping> + 'static,
    G: Gossip + Sync + Send + 'static,
    R: Rpc + Sync + Send + 'static,
    M: MemPool + 'static,
    S: Storage + 'static,
    DB: cita_trie::DB + 'static,
    Mapping: ServiceMapping + 'static,
{
    type CryptoImpl = DefaultCrypto;

    async fn get_block_exec_result(
        &self,
        ctx: Context,
        height: Height,
    ) -> Result<ExecResult<ExecResp>, Box<dyn Error + Send>>{
        Ok(ExecResult::default())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_block(
        &self,
        ctx: Context,
        height: Height,
        exec_height: Height,
        pre_hash: Hash,
        pre_proof: Proof,
        block_states: Vec<BlockState<ExecResp>>,
        last_commit_exec_resp: ExecResp,
    ) -> Result<WrappedPill, Box<dyn Error + Send>> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let (ordered_tx_hashes, propose_hashes) = self
            .mem_pool
            .package(
                ctx,
                last_commit_exec_resp.cycles_limit,
                last_commit_exec_resp.tx_num_limit,
            )
            .await?
            .clap();
        let order_root = Merkle::from_hashes(ordered_tx_hashes.clone()).get_root_hash();

        let mut block_states = block_states;
        block_states.sort_by(|a, b| a.height.partial_cmp(&b.height).unwrap());

        let exec_resp = if block_states.is_empty() {
            last_commit_exec_resp.clone()
        } else {
            block_states.last().unwrap().state.clone()
        };

        let header = BlockHeader {
            chain_id: self.status.chain_id.clone(),
            pre_hash: ProtoHash::from_bytes(pre_hash)?,
            height,
            exec_height,
            timestamp,
            logs_bloom: block_states
                .iter()
                .map(|stat| stat.state.logs_bloom.clone())
                .collect(),
            order_root: order_root.unwrap_or_else(ProtoHash::from_empty),
            confirm_root: block_states
                .iter()
                .map(|stat| stat.state.order_root.clone())
                .collect(),
            state_root: exec_resp.state_root,
            receipt_root: block_states
                .iter()
                .map(|stat| stat.state.receipt_root.clone())
                .collect(),
            cycles_used: block_states
                .iter()
                .map(|stat| stat.state.cycles_used)
                .collect(),
            proposer: self.status.address.clone(),
            proof: into_proto_proof(pre_proof)?,
            validator_version: 0u64,
            validators: last_commit_exec_resp.validators.clone(),
        };

        let block = Block {
            header,
            ordered_tx_hashes,
        };

        let pill = Pill {
            block,
            propose_hashes,
        };

        let wrapped_pill = WrappedPill(pill);

        let mut set = self.status.from_myself.write();
        set.insert(wrapped_pill.get_block_hash()?);

        Ok(wrapped_pill)
    }

    async fn check_block(
        &self,
        _ctx: Context,
        pill: &WrappedPill,
        block_states: &[BlockState<ExecResp>],
        last_commit_exec_resp: &ExecResp,
    ) -> Result<(), Box<dyn Error + Send>> {
        let block_hash = pill.get_block_hash()?;
        if self.status.from_myself.read().contains(&block_hash) {
            return Ok(());
        }

        let expect_order_root = Merkle::from_hashes(
            pill.0
                .block.ordered_tx_hashes
                .iter()
                .map(|r| ProtoHash::digest(r.to_owned().encode_fixed().unwrap()))
                .collect::<Vec<_>>(),
        )
            .get_root_hash()
            .unwrap_or_else(ProtoHash::from_empty);
        if expect_order_root != pill.0.block.header.order_root {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.order_root != expect.order_root".to_owned(),
            )));
        }

        let mut block_states = block_states.to_vec();
        block_states.sort_by(|a, b| a.height.partial_cmp(&b.height).unwrap());

        let exec_resp = if block_states.is_empty() {
            last_commit_exec_resp.clone()
        } else {
            block_states.last().unwrap().state.clone()
        };

        let header = &pill.0.block.header;

        if header.chain_id != self.status.chain_id {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.chain_id != self.chain_id".to_owned(),
            )));
        }

        if header.state_root != exec_resp.state_root {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.state_root != expect.state_root".to_owned(),
            )));
        }

        if header.validators != exec_resp.validators {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.validators != expect.validators".to_owned(),
            )));
        }

        if header.logs_bloom.len() != block_states.len()
            || !header
                .logs_bloom
                .iter()
                .zip(block_states.iter())
                .all(|(a, b)| a == &b.state.logs_bloom)
        {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.log_bloom != expect.log_bloom".to_owned(),
            )));
        }

        if header.receipt_root.len() != block_states.len()
            || !header
                .receipt_root
                .iter()
                .zip(block_states.iter())
                .all(|(a, b)| a == &b.state.receipt_root)
        {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.receipt_root != expect.receipt_root".to_owned(),
            )));
        }

        if header.cycles_used.len() != block_states.len()
            || !header
                .cycles_used
                .iter()
                .zip(block_states.iter())
                .all(|(a, b)| a == &b.state.cycles_used)
        {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.cycles_used != expect.cycles_used".to_owned(),
            )));
        }

        if header.confirm_root.len() != block_states.len()
            || !header
                .confirm_root
                .iter()
                .zip(block_states.iter())
                .all(|(a, b)| a == &b.state.order_root)
        {
            return Err(Box::new(ConsensusError::CheckBlock(
                "block.confirm_root != expect.confirm_root".to_owned(),
            )));
        }

        Ok(())
    }

    async fn fetch_full_block(
        &self,
        ctx: Context,
        pill: WrappedPill,
    ) -> Result<Bytes, Box<dyn Error + Send>> {
        let block_hash = pill.get_block_hash()?;
        let ordered_tx_hashes = pill.0.block.ordered_tx_hashes.clone();
        if !self.status.from_myself.read().contains(&block_hash) {
            self.mem_pool
                .ensure_order_txs(ctx.clone(), ordered_tx_hashes.clone())
                .await?
        }
        let txs = self.mem_pool.get_full_txs(ctx, ordered_tx_hashes).await?;
        let full_block = FullBlock {
            block:       pill.0.block,
            ordered_txs: txs,
        };
        Ok(full_block.encode_fixed()?)
    }

    async fn save_and_exec_block_with_proof(
        &self,
        ctx: Context,
        height: Height,
        full_block: Bytes,
        proof: Proof,
        last_exec_resp: ExecResp,
        last_commit_exec_resp: ExecResp,
    ) -> Result<ExecResult<ExecResp>, Box<dyn Error + Send>> {
        // Todo: this can be removed to promote performance if muta test stable for a long time
        let latest_height = self.storage.get_latest_block().await?.header.height;
        if latest_height != height - 1 {
            panic!("save_and_exec_block_with_proof, latest_height != height - 1, {} != {} - 1", latest_height, height);
        }

        let full_block: FullBlock = FixedCodec::decode_fixed(full_block)?;

        let order_root = full_block.block.header.order_root.clone();
        let state_root = last_exec_resp.state_root;
        let timestamp = full_block.block.header.timestamp;
        let cycles_limit = last_commit_exec_resp.cycles_limit;

        //todo
        let resp = self.exec(state_root.clone(), height, timestamp, cycles_limit, &full_block.ordered_txs)?;
        let metadata = self.get_metadata(ctx.clone(), resp.state_root.clone(), height, timestamp)?;
        let ordered_tx_hashes = full_block.block.ordered_tx_hashes.clone();
        let receipt_root = Merkle::from_hashes(
            resp
                .receipts
                .iter()
                .map(|r| ProtoHash::digest(r.to_owned().encode_fixed().unwrap()))
                .collect::<Vec<_>>(),
        )
            .get_root_hash()
            .unwrap_or_else(ProtoHash::from_empty);

        self.storage.insert_receipts(resp.receipts.clone()).await?;
        self.storage.update_latest_proof(into_proto_proof(proof)?).await?;
        self.storage.insert_block(full_block.block).await?;
        self.storage.insert_transactions(full_block.ordered_txs).await?;
        self.mem_pool.flush(ctx, ordered_tx_hashes).await?;

        let exec_result = create_exec_result(height, metadata, state_root, order_root, receipt_root, resp.logs_bloom, resp.all_cycles_used);
        Ok(exec_result.clone())
    }

    async fn commit(&self, _ctx: Context, _commit_state: ExecResult<ExecResp>) {
        self.status.from_myself.write().clear();
    }

    async fn register_network(
        &self,
        _ctx: Context,
        sender: UnboundedSender<(Context, OverlordMsg<WrappedPill>)>,
    ) {
    }

    async fn broadcast(
        &self,
        ctx: Context,
        msg: OverlordMsg<WrappedPill>,
    ) -> Result<(), Box<dyn Error + Send>> {
        Ok(())
    }

    async fn transmit(
        &self,
        ctx: Context,
        to: Address,
        msg: OverlordMsg<WrappedPill>,
    ) -> Result<(), Box<dyn Error + Send>> {
        Ok(())
    }

    /// should return empty vec if the required blocks are not exist
    async fn get_block_with_proofs(
        &self,
        ctx: Context,
        height_range: HeightRange,
    ) -> Result<Vec<(WrappedPill, Proof)>, Box<dyn Error + Send>> {
        Ok(vec![])
    }

    async fn get_latest_height(&self, ctx: Context) -> Result<Height, Box<dyn Error + Send>> {
        Ok(0)
    }

    async fn handle_error(&self, ctx: Context, err: OverlordError) {}
}

impl<EF, G, M, R, S, DB, Mapping> OverlordAdapter<EF, G, M, R, S, DB, Mapping>
    where
        EF: ExecutorFactory<DB, S, Mapping>,
        G: Gossip + Sync + Send + 'static,
        R: Rpc + Sync + Send + 'static,
        M: MemPool + 'static,
        S: Storage + 'static,
        DB: cita_trie::DB + 'static,
        Mapping: ServiceMapping + 'static,{

    fn get_metadata(
        &self,
        _ctx: Context,
        state_root: MerkleRoot,
        height: u64,
        timestamp: u64,
    ) -> ProtocolResult<Metadata> {
        let executor = EF::from_root(
            state_root.clone(),
            Arc::clone(&self.trie_db),
            Arc::clone(&self.storage),
            Arc::clone(&self.service_mapping),
        )?;

        let caller = ProtoAddress::from_hex("0x0000000000000000000000000000000000000000")?;

        let params = ExecutorParams {
            state_root,
            height,
            timestamp,
            cycles_limit: u64::max_value(),
        };
        let exec_resp = executor.read(&params, &caller, 1, &TransactionRequest {
            service_name: "metadata".to_string(),
            method:       "get_metadata".to_string(),
            payload:      "".to_string(),
        })?;

        Ok(serde_json::from_str(&exec_resp.succeed_data).expect("Decode metadata failed!"))
    }

    fn exec(&self, state_root: MerkleRoot, height: Height, timestamp: u64, cycles_limit: u64, ordered_txs: &[SignedTransaction]) -> ProtocolResult<ExecutorResp> {
        let mut executor = EF::from_root(
            state_root.clone(),
            Arc::clone(&self.trie_db),
            Arc::clone(&self.storage),
            Arc::clone(&self.service_mapping),
        )?;
        let exec_params = ExecutorParams {
            state_root,
            height,
            timestamp,
            cycles_limit,
        };
        executor.exec(&exec_params, ordered_txs)
    }
}

fn create_exec_result(height: Height, metadata: Metadata, state_root: MerkleRoot, order_root: MerkleRoot, receipt_root: MerkleRoot, logs_bloom: Bloom, cycles_used: u64) -> ExecResult<ExecResp> {
    let time_config = TimeConfig {
        interval:         metadata.interval,
        propose_ratio:    metadata.propose_ratio,
        pre_vote_ratio:   metadata.prevote_ratio,
        pre_commit_ratio: metadata.precommit_ratio,
        brake_ratio:      metadata.brake_ratio,
    };

   let auth_config = AuthConfig {
        common_ref: metadata.common_ref.as_string(),
        mode:       SelectMode::InTurn,
        auth_list:  to_overlord_auth_list(&metadata.verifier_list),
    };

    let consensus_config = OverlordConfig {
        max_exec_behind: 5,
        auth_config,
        time_config,
    };

    let exec_resp = ExecResp {
        order_root,
        state_root,
        receipt_root,
        cycles_used,
        logs_bloom,
        cycles_limit: metadata.cycles_limit,
        tx_num_limit: metadata.tx_num_limit,
        max_tx_size:  metadata.max_tx_size,
        validators:   to_validator_list(&metadata.verifier_list),
    };

    ExecResult {
        consensus_config,
        block_states: BlockState{
            height,
            state: exec_resp,
        }
    }
}

fn to_overlord_auth_list(validators: &[ValidatorExtend]) -> Vec<Node> {
    validators
        .iter()
        .map(|v| Node {
            address:        v.address.as_bytes(),
            party_pub_key:  v.bls_pub_key.as_string(),
            propose_weight: v.propose_weight,
            vote_weight:    v.vote_weight,
        })
        .collect::<Vec<_>>()
}

fn to_validator_list(validators: &[ValidatorExtend]) -> Vec<Validator> {
    validators
        .iter()
        .map(|v| Validator {
            address:        v.address.clone(),
            propose_weight: v.propose_weight,
            vote_weight:    v.vote_weight,
        })
        .collect::<Vec<_>>()
}

#[derive(Clone, Debug, Default, Display, PartialEq, Eq)]
#[display(fmt = "{{ chain_id: {}, height: {}, exec_height: {}, order_tx_len: {}, propose_tx_len: {}, pre_hash: {}, timestamp: {}, state_root: {}, order_root: {}, confirm_root: {:?}, cycle_used: {:?}, proposer: {}, validator_version: {}, validators: {:?} }}",
"_0.block.header.chain_id.as_bytes().tiny_hex()", 
"_0.block.header.height",
"_0.block.header.exec_height",
"_0.block.ordered_tx_hashes.len()",
"_0.propose_hashes.len()",
"_0.block.header.pre_hash.as_bytes().tiny_hex()",
"_0.block.header.timestamp",
"_0.block.header.state_root.as_bytes().tiny_hex()",
"_0.block.header.order_root.as_bytes().tiny_hex()",
"_0.block.header.confirm_root.iter().map(|root| root.as_bytes().tiny_hex()).collect::<Vec<String>>()",
"_0.block.header.cycles_used",
"_0.block.header.proposer.as_bytes().tiny_hex()",
"_0.block.header.validator_version",
"_0.block.header.validators.iter().map(|v| format!(\"{{ address: {}, propose_w: {}, vote_w: {} }}\", v.address.as_bytes().tiny_hex(), v.propose_weight, v.vote_weight))",)]
struct WrappedPill(Pill);

impl Blk for WrappedPill {
    fn fixed_encode(&self) -> Result<Bytes, Box<dyn Error + Send>> {
        let encode = self.0.encode_fixed()?;
        Ok(encode)
    }

    fn fixed_decode(data: &Bytes) -> Result<Self, Box<dyn Error + Send>> {
        let pill = FixedCodec::decode_fixed(data.clone())?;
        Ok(WrappedPill(pill))
    }

    fn get_block_hash(&self) -> Result<Hash, Box<dyn Error + Send>> {
        Ok(ProtoHash::digest(self.0.block.encode_fixed()?).as_bytes())
    }

    fn get_pre_hash(&self) -> Hash {
        self.0.block.header.pre_hash.as_bytes()
    }

    fn get_height(&self) -> Height {
        self.0.block.header.height
    }

    fn get_exec_height(&self) -> Height {
        self.0.block.header.exec_height
    }

    fn get_proof(&self) -> Proof {
        into_proof(self.0.block.header.proof.clone())
    }
}

#[derive(Clone, Debug, Default, Display)]
#[display(
    fmt = "{{ order_root: {}, state_root: {}, receipt_root: {}, cycle_used: {} }}",
    "order_root.as_bytes().tiny_hex()",
    "state_root.as_bytes().tiny_hex()",
    "receipt_root.as_bytes().tiny_hex()",
    cycles_used
)]
struct ExecResp {
    order_root:   MerkleRoot,
    state_root:   MerkleRoot,
    receipt_root: MerkleRoot,
    cycles_used:  u64,
    logs_bloom:   Bloom,
    cycles_limit: u64,
    tx_num_limit: u64,
    max_tx_size:  u64,
    validators:   Vec<Validator>,
}

impl St for ExecResp {}

fn into_proof(proof: ProtoProof) -> Proof {
    let vote = Vote::new(proof.height, proof.round, proof.block_hash.as_bytes());
    let aggregates = Aggregates::new(proof.bitmap, proof.signature);
    Proof::new(vote, aggregates)
}

fn into_proto_proof(proof: Proof) -> ProtocolResult<ProtoProof> {
    let proof = ProtoProof {
        height:     proof.vote.height,
        round:      proof.vote.round,
        block_hash: ProtoHash::from_bytes(proof.vote.block_hash)?,
        signature:  proof.aggregates.signature,
        bitmap:     proof.aggregates.address_bitmap,
    };
    Ok(proof)
}

#[derive(Debug, Display)]
pub enum ConsensusError {
    #[display(fmt = "check block failed, {}", _0)]
    CheckBlock(String),
}

impl Error for ConsensusError {}

impl From<ConsensusError> for ProtocolError {
    fn from(err: ConsensusError) -> ProtocolError {
        ProtocolError::new(ProtocolErrorKind::Consensus, Box::new(err))
    }
}
