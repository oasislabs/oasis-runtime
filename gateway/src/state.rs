use std::{mem,
          ops,
          sync::{Arc, Mutex}};

use bytes::Bytes;
use ethcore;
//use ethcore::blockchain::extras;
use ethcore::db::{self, Key, Readable};
use ethcore::encoded;
use ethcore::header::{BlockNumber, Header};
use ethcore::state::backend::Basic as BasicBackend;
use ethereum_types::{H256, U256};
use journaldb::overlaydb::OverlayDB;
use kvdb::{self, KeyValueDB};
use rlp_compress::{blocks_swapper, decompress};

use client_utils::db::Snapshot;
use ekiden_core;
use ekiden_core::error::Result;
use ekiden_db_trusted::Database;

pub struct StateDb {
    snapshot: Snapshot,
}

impl StateDb {
    pub fn new(snapshot: Snapshot) -> Option<Self> {
        let db = Self { snapshot: snapshot };
        match db.best_block_hash() {
            Some(_) => Some(db),
            None => None,
        }
    }

    pub fn best_block_hash(&self) -> Option<H256> {
        match self.get(db::COL_EXTRA, b"best") {
            Ok(best) => best.map(|best| H256::from_slice(&best)),
            Err(_) => None,
        }
    }

    fn block_header_data(&self, hash: &H256) -> Option<encoded::Header> {
        self.get(db::COL_HEADERS, &hash)
            .unwrap()
            .map(|h| encoded::Header::new(decompress(&h, blocks_swapper()).into_vec()))
    }

    fn block_body(&self, hash: &H256) -> Option<encoded::Body> {
        // Read from DB and populate cache
        let b = self.get(db::COL_BODIES, hash)
            .expect("Low level database error. Some issue with disk?")?;
        let body = encoded::Body::new(decompress(&b, blocks_swapper()).into_vec());
        Some(body)
    }

    pub fn block(&self, hash: &H256) -> Option<encoded::Block> {
        let header = self.block_header_data(hash)?;
        let body = self.block_body(hash)?;
        Some(encoded::Block::new_from_header_and_body(
            &header.view(),
            &body.view(),
        ))
    }

    pub fn block_hash(&self, index: BlockNumber) -> Option<H256> {
        self.read(db::COL_EXTRA, &index)
    }

    // convenience function
    pub fn best_block_state_root(&self) -> Option<H256> {
        match self.best_block_hash() {
            Some(hash) => self.block_header_data(&hash)
                .map(|h| h.state_root().clone()),
            None => None,
        }
    }

    pub fn best_block_number(&self) -> BlockNumber {
        match self.best_block_hash() {
            Some(hash) => self.block_header_data(&hash)
                .map(|h| h.number())
                .unwrap_or(0),
            None => 0,
        }
    }
}

type Backend = BasicBackend<OverlayDB>;
pub type EthState = ethcore::state::State<Backend>;

pub fn get_ethstate(snapshot: Snapshot) -> Option<EthState> {
    if let Some(db) = StateDb::new(snapshot) {
        let root = db.best_block_state_root().unwrap();
        let backend = BasicBackend(OverlayDB::new(Arc::new(db), None /* col */));
        Some(
            ethcore::state::State::from_existing(
                backend,
                root,
                U256::zero(),       /* account_start_nonce */
                Default::default(), /* factories */
            ).unwrap(),
        )
    } else {
        None
    }
}

pub fn to_bytes(num: u32) -> [u8; mem::size_of::<u32>()] {
    unsafe { mem::transmute(num) }
}

// parity expects the database to namespace keys by column
// the ekiden db doesn't [yet?] have this feature, so we emulate by
// prepending the column id to the actual key
fn get_key(col: Option<u32>, key: &[u8]) -> Vec<u8> {
    let col_bytes = col.map(|id| to_bytes(id.to_le())).unwrap_or([0, 0, 0, 0]);
    col_bytes
        .into_iter()
        .chain(key.into_iter())
        .map(|v| v.to_owned())
        .collect()
}

impl kvdb::KeyValueDB for StateDb {
    fn get(&self, col: Option<u32>, key: &[u8]) -> kvdb::Result<Option<kvdb::DBValue>> {
        Ok(self.snapshot
            .get(&get_key(col, key))
            .map(kvdb::DBValue::from_vec))
    }

    fn get_by_prefix(&self, _col: Option<u32>, _prefix: &[u8]) -> Option<Box<[u8]>> {
        unimplemented!();
    }

    fn write_buffered(&self, transaction: kvdb::DBTransaction) {
        unimplemented!();
    }

    fn flush(&self) -> kvdb::Result<()> {
        unimplemented!();
    }

    fn iter<'a>(&'a self, _col: Option<u32>) -> Box<Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        unimplemented!();
    }

    fn iter_from_prefix<'a>(
        &'a self,
        _col: Option<u32>,
        _prefix: &'a [u8],
    ) -> Box<Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        unimplemented!();
    }

    fn restore(&self, _new_db: &str) -> kvdb::Result<()> {
        unimplemented!();
    }
}
