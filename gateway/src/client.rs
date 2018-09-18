use std::marker::{Send, Sync};
use std::sync::{Arc, Mutex, RwLock, Weak};

use bytes::Bytes;
use common_types::log_entry::LocalizedLogEntry;
use ethcore::blockchain::{BlockProvider, TransactionAddress};
use ethcore::client::{BlockId, EnvInfo, LastHashes, TransactionId};
use ethcore::encoded;
use ethcore::engines::EthEngine;
use ethcore::error::CallError;
use ethcore::executive::{contract_address, Executed, Executive, TransactOptions};
use ethcore::filter::Filter as EthcoreFilter;
use ethcore::header::BlockNumber;
use ethcore::receipt::LocalizedReceipt;
use ethcore::spec::Spec;
use ethereum_types::{Address, H256, U256};
use futures::future::Future;
use runtime_ethereum;
use transaction::{Action, LocalizedTransaction, SignedTransaction};

use client_utils;
#[cfg(feature = "read_state")]
use client_utils::db::Snapshot;
use ekiden_core::error::Error;
#[cfg(feature = "read_state")]
use ekiden_db_trusted::Database;
use ekiden_storage_base::StorageBackend;
#[cfg(not(feature = "read_state"))]
use ethereum_api::{Filter, Log, Receipt, Transaction, TransactionRequest};

#[cfg(feature = "read_state")]
use state::{self, EthState, StateDb};
use storage::Web3GlobalStorage;
use util::from_block_id;

// record contract call outcome
fn contract_call_result<T>(call: &str, result: Result<T, Error>, default: T) -> T {
    match result {
        Ok(val) => {
            measure_counter_inc!("contract_call_succeeded");
            val
        }
        Err(e) => {
            measure_counter_inc!("contract_call_failed");
            error!("{}: {:?}", call, e);
            default
        }
    }
}

/// An actor listening to chain events.
pub trait ChainNotify: Send + Sync {
    fn has_heads_subscribers(&self) -> bool;

    /// Notifies about new headers.
    fn notify_heads(&self, headers: &[encoded::Header]);

    /// Notifies about new log filter matches.
    fn notify_logs(&self, from_block: BlockId, to_block: BlockId);
}

pub struct Client {
    client: runtime_ethereum::Client,
    engine: Arc<EthEngine>,
    snapshot_manager: Option<client_utils::db::Manager>,
    eip86_transition: u64,
    storage: Arc<RwLock<Web3GlobalStorage>>,
    /// The most recent block for which we have sent notifications.
    notified_block_number: Mutex<BlockNumber>,
    listeners: RwLock<Vec<Weak<ChainNotify>>>,
}

impl Client {
    pub fn new(
        spec: &Spec,
        snapshot_manager: Option<client_utils::db::Manager>,
        client: runtime_ethereum::Client,
        backend: Arc<StorageBackend>,
    ) -> Self {
        let storage = Web3GlobalStorage::new(backend);
        Self {
            client: client,
            engine: spec.engine.clone(),
            snapshot_manager: snapshot_manager,
            eip86_transition: spec.params().eip86_transition,
            storage: Arc::new(RwLock::new(storage)),
            // TODO: initialize to current block number
            notified_block_number: Mutex::new(0),
            listeners: RwLock::new(vec![]),
        }
    }

    /// Notify listeners of new blocks.
    pub fn new_blocks(&self) {
        const MAX_HEADERS: u64 = 256;

        let mut last_block = self.notified_block_number.lock().unwrap();

        if let Some(db) = self.get_db_snapshot() {
            let current_block = db.best_block_number();
            if current_block > *last_block {
                self.notify(|listener| {
                    // optimization: only generate the list of headers if we have subscribers
                    if listener.has_heads_subscribers() {
                        // notify listeners of up to 256 most recent headers since last notification
                        let headers =
                            Self::headers_since(&db, *last_block + 1, current_block, MAX_HEADERS);
                        listener.notify_heads(&headers);
                    }

                    // notify log listeners of blocks last+1...current
                    listener.notify_logs(
                        BlockId::Number(*last_block + 1),
                        BlockId::Number(current_block),
                    );
                });

                // update last notified block
                *last_block = current_block;
            }
        }
    }

    /// Adds a new `ChainNotify` listener.
    pub fn add_listener(&self, listener: Weak<ChainNotify>) {
        self.listeners.write().unwrap().push(listener);
    }

    /// Notify `ChainNotify` listeners.
    fn notify<F: Fn(&ChainNotify)>(&self, f: F) {
        for listener in &*self.listeners.read().unwrap() {
            if let Some(listener) = listener.upgrade() {
                f(&*listener)
            }
        }
    }

    /// Returns the BlockId corresponding to the larger block number.
    pub fn max_block_number(&self, id_a: BlockId, id_b: BlockId) -> BlockId {
        // first check if either is Latest
        if id_a == BlockId::Latest || id_b == BlockId::Latest {
            return BlockId::Latest;
        }

        // if either is Earliest, return the other
        if id_a == BlockId::Earliest {
            return id_b;
        }
        if id_b == BlockId::Earliest {
            return id_a;
        }

        // at this point, we need block numbers
        let to_block_number = |id| match id {
            BlockId::Latest => unreachable!(),
            BlockId::Earliest => unreachable!(),
            BlockId::Number(num) => num,
            BlockId::Hash(hash) => match self.block_number(hash) {
                Some(num) => num,
                None => 0,
            },
        };

        if to_block_number(id_a) > to_block_number(id_b) {
            id_a
        } else {
            id_b
        }
    }

    /// Returns the BlockId corresponding to the smaller block number.
    pub fn min_block_number(&self, id_a: BlockId, id_b: BlockId) -> BlockId {
        // first check if either is Earliest
        if id_a == BlockId::Earliest || id_b == BlockId::Earliest {
            return BlockId::Earliest;
        }

        // if either is Latest, return the other
        if id_a == BlockId::Latest {
            return id_b;
        }
        if id_b == BlockId::Latest {
            return id_a;
        }

        // at this point, we need block numbers
        let to_block_number = |id| match id {
            BlockId::Latest => unreachable!(),
            BlockId::Earliest => unreachable!(),
            BlockId::Number(num) => num,
            BlockId::Hash(hash) => match self.block_number(hash) {
                Some(num) => num,
                None => 0,
            },
        };

        if to_block_number(id_a) < to_block_number(id_b) {
            id_a
        } else {
            id_b
        }
    }

    /// Block number at which EIP-86 transition occurs.
    /// https://github.com/ethereum/EIPs/blob/master/EIPS/eip-86.md
    pub fn eip86_transition(&self) -> u64 {
        self.eip86_transition
    }

    /// Returns a StateDb backed by an Ekiden db snapshot, or None when the
    /// blockchain database has not yet been initialized by the runtime.
    #[cfg(feature = "read_state")]
    fn get_db_snapshot(&self) -> Option<StateDb<Snapshot>> {
        match self.snapshot_manager {
            Some(ref manager) => {
                let ret = state::StateDb::new(manager.get_snapshot());
                if ret.is_none() {
                    measure_counter_inc!("read_state_failed");
                    error!("Could not get db snapshot");
                }
                ret
            }
            None => None,
        }
    }

    // block-related
    pub fn best_block_number(&self) -> BlockNumber {
        #[cfg(feature = "read_state")]
        {
            if let Some(db) = self.get_db_snapshot() {
                return db.best_block_number();
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result(
            "get_block_height",
            self.client.get_block_height(false).wait(),
            U256::from(0),
        ).into()
    }

    pub fn block(&self, id: BlockId) -> Option<encoded::Block> {
        #[cfg(feature = "read_state")]
        {
            if let Some(db) = self.get_db_snapshot() {
                return self.block_hash(id).and_then(|h| db.block(&h));
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result::<Option<Vec<u8>>>(
            "get_block",
            self.client.get_block(from_block_id(id)).wait(),
            None,
        ).map(|block| encoded::Block::new(block))
    }

    #[cfg(feature = "read_state")]
    pub fn block_hash(&self, id: BlockId) -> Option<H256> {
        if let BlockId::Hash(hash) = id {
            Some(hash)
        } else {
            if let Some(db) = self.get_db_snapshot() {
                match id {
                    BlockId::Hash(_hash) => unreachable!(),
                    BlockId::Number(number) => db.block_hash(number),
                    BlockId::Earliest => db.block_hash(0),
                    BlockId::Latest => db.best_block_hash(),
                }
            } else {
                None
            }
        }
    }

    #[cfg(not(feature = "read_state"))]
    pub fn block_hash(&self, id: BlockId) -> Option<H256> {
        if let BlockId::Hash(hash) = id {
            Some(hash)
        } else {
            contract_call_result(
                "get_block_hash",
                self.client.get_block_hash(from_block_id(id)).wait(),
                None,
            )
        }
    }

    #[cfg(feature = "read_state")]
    fn block_number(&self, hash: H256) -> Option<BlockNumber> {
        if let Some(db) = self.get_db_snapshot() {
            db.block_number(&hash)
        } else {
            None
        }
    }

    // transaction-related
    #[cfg(feature = "read_state")]
    pub fn transaction(&self, id: TransactionId) -> Option<LocalizedTransaction> {
        if let Some(db) = self.get_db_snapshot() {
            let address = match id {
                TransactionId::Hash(ref hash) => db.transaction_address(hash),
                TransactionId::Location(id, index) => {
                    Self::id_to_block_hash(&db, id).map(|hash| TransactionAddress {
                        block_hash: hash,
                        index: index,
                    })
                }
            };
            address.and_then(|addr| db.transaction(&addr))
        } else {
            None
        }
    }

    #[cfg(not(feature = "read_state"))]
    pub fn transaction(&self, hash: H256) -> Option<Transaction> {
        contract_call_result(
            "get_transaction",
            self.client.get_transaction(hash).wait(),
            None,
        )
    }

    #[cfg(feature = "read_state")]
    pub fn transaction_receipt(&self, hash: H256) -> Option<LocalizedReceipt> {
        if let Some(db) = self.get_db_snapshot() {
            let address = db.transaction_address(&hash)?;
            let receipt = db.transaction_receipt(&address)?;
            let mut tx = db.transaction(&address)?;

            let transaction_hash = tx.hash();
            let block_hash = tx.block_hash;
            let block_number = tx.block_number;
            let transaction_index = tx.transaction_index;

            Some(LocalizedReceipt {
                transaction_hash: transaction_hash,
                transaction_index: transaction_index,
                block_hash: block_hash,
                block_number: block_number,
                cumulative_gas_used: receipt.gas_used,
                gas_used: receipt.gas_used,
                contract_address: match tx.action {
                    Action::Call(_) => None,
                    Action::Create => Some(
                        contract_address(
                            self.engine.create_address_scheme(block_number),
                            &tx.sender(),
                            &tx.nonce,
                            &tx.data,
                        ).0,
                    ),
                },
                logs: receipt
                    .logs
                    .into_iter()
                    .enumerate()
                    .map(|(i, log)| LocalizedLogEntry {
                        entry: log,
                        block_hash: block_hash,
                        block_number: block_number,
                        transaction_hash: transaction_hash,
                        transaction_index: transaction_index,
                        transaction_log_index: i,
                        log_index: i,
                    })
                    .collect(),
                log_bloom: receipt.log_bloom,
                outcome: receipt.outcome,
            })
        } else {
            None
        }
    }

    #[cfg(not(feature = "read_state"))]
    pub fn transaction_receipt(&self, hash: H256) -> Option<Receipt> {
        contract_call_result("get_receipt", self.client.get_receipt(hash).wait(), None)
    }

    #[cfg(feature = "read_state")]
    fn id_to_block_hash<T>(db: &StateDb<T>, id: BlockId) -> Option<H256>
    where
        T: 'static + Database + Send + Sync,
    {
        match id {
            BlockId::Hash(hash) => Some(hash),
            BlockId::Number(number) => db.block_hash(number),
            BlockId::Earliest => db.block_hash(0),
            BlockId::Latest => db.best_block_hash(),
        }
    }

    #[cfg(feature = "read_state")]
    pub fn logs(&self, filter: EthcoreFilter) -> Vec<LocalizedLogEntry> {
        if let Some(db) = self.get_db_snapshot() {
            let fetch_logs = || {
                let from_hash = Self::id_to_block_hash(&db, filter.from_block)?;
                let from_number = db.block_number(&from_hash)?;
                // NOTE: there appears to be a bug in parity with to_hash:
                // https://github.com/ekiden/parity/blob/master/ethcore/src/client/client.rs#L1856
                let to_hash = Self::id_to_block_hash(&db, filter.to_block)?;

                let blooms = filter.bloom_possibilities();
                let bloom_match = |header: &encoded::Header| {
                    blooms
                        .iter()
                        .any(|bloom| header.log_bloom().contains_bloom(bloom))
                };

                let (blocks, last_hash) = {
                    let mut blocks = Vec::new();
                    let mut current_hash = to_hash;

                    loop {
                        let header = db.block_header_data(&current_hash)?;
                        if bloom_match(&header) {
                            blocks.push(current_hash);
                        }

                        // Stop if `from` block is reached.
                        if header.number() <= from_number {
                            break;
                        }
                        current_hash = header.parent_hash();
                    }

                    blocks.reverse();
                    (blocks, current_hash)
                };

                // Check if we've actually reached the expected `from` block.
                if last_hash != from_hash || blocks.is_empty() {
                    return None;
                }

                Some(db.logs(blocks, |entry| filter.matches(entry), filter.limit))
            };

            fetch_logs().unwrap_or_default()
        } else {
            vec![]
        }
    }

    #[cfg(not(feature = "read_state"))]
    pub fn logs(&self, filter: EthcoreFilter) -> Vec<Log> {
        let filter = Filter {
            from_block: from_block_id(filter.from_block),
            to_block: from_block_id(filter.to_block),
            address: match filter.address {
                Some(address) => Some(address.into_iter().map(Into::into).collect()),
                None => None,
            },
            topics: filter.topics.into_iter().map(Into::into).collect(),
            limit: filter.limit.map(Into::into),
        };
        contract_call_result("get_logs", self.client.get_logs(filter).wait(), vec![])
    }

    // account state-related

    /// Returns an EthState at the specified BlockId, backed by an Ekiden db
    /// snapshot, or None when the blockchain database has not yet been
    /// initialized by the runtime.
    #[cfg(feature = "read_state")]
    fn get_ethstate_snapshot_at(&self, id: BlockId) -> Option<EthState> {
        self.get_db_snapshot()?.get_ethstate_at(id)
    }

    pub fn balance(&self, address: &Address, id: BlockId) -> Option<U256> {
        #[cfg(feature = "read_state")]
        {
            if let Some(state) = self.get_ethstate_snapshot_at(id) {
                match state.balance(&address) {
                    Ok(balance) => return Some(balance),
                    Err(e) => {
                        measure_counter_inc!("read_state_failed");
                        error!("Could not get balance from ethstate: {:?}", e);
                        return None;
                    }
                }
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result(
            "get_account_balance",
            self.client.get_account_balance(*address).wait().map(Some),
            None,
        )
    }

    pub fn code(&self, address: &Address, id: BlockId) -> Option<Option<Bytes>> {
        // TODO: differentiate between no account vs no code?
        #[cfg(feature = "read_state")]
        {
            if let Some(state) = self.get_ethstate_snapshot_at(id) {
                match state.code(&address) {
                    Ok(code) => return Some(code.map(|c| (&*c).clone())),
                    Err(e) => {
                        measure_counter_inc!("read_state_failed");
                        error!("Could not get code from ethstate: {:?}", e);
                        return None;
                    }
                }
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result(
            "get_account_code",
            self.client.get_account_code(*address).wait().map(Some),
            None,
        )
    }

    pub fn nonce(&self, address: &Address, id: BlockId) -> Option<U256> {
        #[cfg(feature = "read_state")]
        {
            if let Some(state) = self.get_ethstate_snapshot_at(id) {
                match state.nonce(&address) {
                    Ok(nonce) => return Some(nonce),
                    Err(e) => {
                        measure_counter_inc!("read_state_failed");
                        error!("Could not get nonce from ethstate: {:?}", e);
                        return None;
                    }
                }
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result(
            "get_account_nonce",
            self.client.get_account_nonce(*address).wait().map(Some),
            None,
        )
    }

    pub fn storage_at(&self, address: &Address, position: &H256, id: BlockId) -> Option<H256> {
        #[cfg(feature = "read_state")]
        {
            if let Some(state) = self.get_ethstate_snapshot_at(id) {
                match state.storage_at(address, position) {
                    Ok(val) => return Some(val),
                    Err(e) => {
                        measure_counter_inc!("read_state_failed");
                        error!("Could not get storage from ethstate: {:?}", e);
                        return None;
                    }
                }
            }
        }
        // fall back to contract call if database has not been initialized
        contract_call_result(
            "get_storage_at",
            self.client
                .get_storage_at((*address, *position))
                .wait()
                .map(Some),
            None,
        )
    }

    #[cfg(feature = "read_state")]
    fn last_hashes<T>(db: &StateDb<T>, parent_hash: &H256) -> Arc<LastHashes>
    where
        T: 'static + Database + Send + Sync,
    {
        let mut last_hashes = LastHashes::new();
        last_hashes.resize(256, H256::default());
        last_hashes[0] = parent_hash.clone();
        for i in 0..255 {
            match db.block_details(&last_hashes[i]) {
                Some(details) => {
                    last_hashes[i + 1] = details.parent.clone();
                }
                None => break,
            }
        }
        Arc::new(last_hashes)
    }

    /// Returns a vector of block headers from block numbers start...end (inclusive).
    /// Limited to the `max` most recent headers.
    #[cfg(feature = "read_state")]
    fn headers_since<T>(
        db: &StateDb<T>,
        start: BlockNumber,
        end: BlockNumber,
        max: u64,
    ) -> Vec<encoded::Header>
    where
        T: 'static + Database + Send + Sync,
    {
        // limit to `max` headers
        let start = if end - start + 1 >= max {
            end - max + 1
        } else {
            start
        };

        let mut head = db.block_hash(end)
            .and_then(|hash| db.block_header_data(&hash))
            .expect("Chain is corrupt");

        let mut headers = Vec::with_capacity((end - start + 1) as usize);

        loop {
            headers.push(head.clone());
            if head.number() <= start {
                break;
            }
            head = db.block_header_data(&head.parent_hash())
                .expect("Chain is corrupt");
        }
        headers.reverse();
        headers
    }

    #[cfg(feature = "read_state")]
    fn get_env_info<T>(db: &StateDb<T>) -> EnvInfo
    where
        T: 'static + Database + Send + Sync,
    {
        let parent = db.best_block_hash()
            .and_then(|hash| db.block_header_data(&hash))
            .expect("No best block");
        EnvInfo {
            // next block
            number: parent.number() + 1,
            author: Address::default(),
            timestamp: parent.timestamp(),
            difficulty: U256::zero(),
            last_hashes: Self::last_hashes(db, &parent.hash()),
            gas_used: U256::default(),
            gas_limit: U256::max_value(),
        }
    }

    // transaction-related
    #[cfg(feature = "read_state")]
    pub fn call(
        &self,
        transaction: &SignedTransaction,
        id: BlockId,
    ) -> Result<Executed, CallError> {
        let db = match self.get_db_snapshot() {
            Some(db) => db,
            None => {
                error!("Could not get db snapshot");
                return Err(CallError::StateCorrupt);
            }
        };
        let mut state = match db.get_ethstate_at(id) {
            Some(state) => state,
            None => {
                error!("Could not get state snapshot");
                return Err(CallError::StateCorrupt);
            }
        };

        let env_info = Self::get_env_info(&db);
        let machine = self.engine.machine();
        let options = TransactOptions::with_no_tracing()
            .dont_check_nonce()
            .save_output_from_contract();
        let ret = Executive::new(
            &mut state,
            &env_info,
            machine,
            &*self.storage.read().unwrap(),
        ).transact_virtual(transaction, options)?;
        Ok(ret)
    }

    #[cfg(not(feature = "read_state"))]
    pub fn call(&self, request: TransactionRequest, _id: BlockId) -> Result<Bytes, String> {
        contract_call_result(
            "simulate_transaction",
            self.client
                .simulate_transaction(request)
                .wait()
                .map(|r| r.result),
            Err("no response from runtime".to_string()),
        )
    }

    #[cfg(feature = "read_state")]
    pub fn estimate_gas(
        &self,
        transaction: &SignedTransaction,
        id: BlockId,
    ) -> Result<U256, CallError> {
        let db = match self.get_db_snapshot() {
            Some(db) => db,
            None => {
                error!("Could not get db snapshot");
                return Err(CallError::StateCorrupt);
            }
        };
        let mut state = match db.get_ethstate_at(id) {
            Some(state) => state,
            None => {
                error!("Could not get state snapshot");
                return Err(CallError::StateCorrupt);
            }
        };

        let env_info = Self::get_env_info(&db);
        let machine = self.engine.machine();
        let options = TransactOptions::with_no_tracing()
            .dont_check_nonce()
            .save_output_from_contract();
        let ret = Executive::new(
            &mut state,
            &env_info,
            machine,
            &*self.storage.read().unwrap(),
        ).transact_virtual(transaction, options)?;
        Ok(ret.gas_used + ret.refunded)
    }

    #[cfg(not(feature = "read_state"))]
    pub fn estimate_gas(&self, request: TransactionRequest, _id: BlockId) -> Result<U256, String> {
        contract_call_result(
            "simulate_transaction",
            self.client
                .simulate_transaction(request)
                .wait()
                .map(|r| Ok(r.used_gas + r.refunded_gas)),
            Err("no response from runtime".to_string()),
        )
    }

    pub fn send_raw_transaction(&self, raw: Bytes) -> Result<H256, String> {
        contract_call_result(
            "execute_raw_transaction",
            self.client.execute_raw_transaction(raw).wait().map(|r| {
                if r.created_contract {
                    measure_counter_inc!("contract_created")
                }
                r.hash
            }),
            Err("no response from runtime".to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethereum_types::{Address, H256};
    #[cfg(feature = "read_state")]
    use test_helpers::MockDb;

    #[test]
    #[cfg(feature = "read_state")]
    fn test_last_hashes() {
        let mut db = MockDb::new();
        // populate the db with test data
        db.populate();

        // get state
        let state = StateDb::new(db).unwrap();

        // start with best block
        let hashes = Client::last_hashes(
            &state,
            &H256::from("339ddee2b78be3e53af2b0a3148643973cf0e0fa98e16ab963ee17bf79e6f199"),
        );

        assert_eq!(
            hashes[0],
            H256::from("339ddee2b78be3e53af2b0a3148643973cf0e0fa98e16ab963ee17bf79e6f199")
        );
        assert_eq!(
            hashes[1],
            H256::from("c57db28f3a012eb2a783cd1295a0c5e7fcc08565c526c2c86c8355a54ab7aae3")
        );
        assert_eq!(
            hashes[2],
            H256::from("17a7a94ad21879641349b6e90ccd7e42e63551ad81b3fda561cd2df4860fbd3f")
        );
        assert_eq!(
            hashes[3],
            H256::from("d56eee931740bb35eb9bf9f97cfebb66ac51a1d88988c1255b52677b958d658b")
        );
        assert_eq!(
            hashes[4],
            H256::from("f39c325375fa2d5381a950850abd9999abd2ff64cd0f184139f5bb5d74afb14e")
        );
        assert_eq!(hashes[5], H256::zero());
    }

    #[test]
    #[cfg(feature = "read_state")]
    fn test_envinfo() {
        let mut db = MockDb::new();
        // populate the db with test data
        db.populate();

        // get state
        let state = StateDb::new(db).unwrap();

        let envinfo = Client::get_env_info(&state);
        assert_eq!(envinfo.number, 5);
        assert_eq!(envinfo.author, Address::default());
        assert_eq!(envinfo.timestamp, 0);
        assert_eq!(envinfo.difficulty, U256::zero());
        assert_eq!(
            envinfo.last_hashes[0],
            H256::from("339ddee2b78be3e53af2b0a3148643973cf0e0fa98e16ab963ee17bf79e6f199")
        );
    }
}
