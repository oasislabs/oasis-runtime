#![feature(use_extern_macros)]

extern crate ethbloom;
extern crate ethereum_types;
extern crate jsonrpc_core;
extern crate jsonrpc_http_server;
#[macro_use]
extern crate jsonrpc_macros;
extern crate lazy_static;
extern crate log;
extern crate pretty_env_logger;
extern crate rlp;
extern crate secp256k1;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate sha3;

extern crate hex;
extern crate rustc_hex;

mod error;
mod rpc;
mod util;

use std::{collections::HashMap, fs::File, io::BufReader, sync::Arc};

// Ekiden client packages

#[macro_use]
extern crate clap;
extern crate futures;
extern crate grpcio;
extern crate rand;

#[macro_use]
extern crate client_utils;
extern crate ekiden_contract_client;
extern crate ekiden_core;
extern crate ekiden_rpc_client;

extern crate evm_api;

use clap::{App, Arg};
use futures::future::Future;
use std::fs;

use ekiden_contract_client::create_contract_client;
use ekiden_core::{
  bytes::B256, ring::signature::Ed25519KeyPair, signature::InMemorySigner, untrusted,
};

use ethereum_types::{Address, H256, U256};
use evm_api::{with_api, AccountState, InitStateRequest};
use std::str::FromStr;

use log::{info, log, warn, LevelFilter};

with_api! {
    create_contract_client!(evm, evm_api, api);
}

/// Generate client key pair.
fn create_key_pair() -> Arc<InMemorySigner> {
  let key_pair =
    Ed25519KeyPair::from_seed_unchecked(untrusted::Input::from(&B256::random())).unwrap();
  Arc::new(InMemorySigner::new(key_pair))
}

#[derive(Serialize, Deserialize, Debug)]
struct Account {
  nonce: String,
  balance: String,
  storage: HashMap<String, String>,
  code: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct AccountMap {
  accounts: HashMap<String, Account>,
}

fn main() {
  let known_components = client_utils::components::create_known_components();
  let args = default_app!()
    .args(&known_components.get_arguments())
    .arg(
      Arg::with_name("threads")
        .long("threads")
        .help("Number of threads to use for HTTP server.")
        .default_value("1")
        .takes_value(true),
    )
    .get_matches();

  // reset max log level to Info after default_app macro sets it to Trace
  log::set_max_level(LevelFilter::Info);

  // Initialize component container.
  let mut container = known_components
    .build_with_arguments(&args)
    .expect("failed to initialize component container");

  let signer = create_key_pair();
  let client = contract_client!(signer, evm, args, container);

  let is_genesis_initialized = client.genesis_block_initialized(true).wait().unwrap();
  if is_genesis_initialized {
    warn!("Genesis block already initialized");
  } else {
    init_genesis_block(&client);
  }

  let client_arc = Arc::new(client);
  let addr = "0.0.0.0:8545".parse().unwrap();
  let num_threads = value_t!(args, "threads", usize).unwrap();
  rpc::rpc_loop(client_arc, &addr, num_threads);
}

fn init_genesis_block(client: &evm::Client) {
  info!("Initializing genesis block");
  let mut account_request = Vec::new();
  let mut storage_request = Vec::new();

  // Read in all the files in resources/genesis/
  for path in fs::read_dir("../resources/genesis").unwrap() {
    let path = path.unwrap().path();
    let br = BufReader::new(File::open(path.clone()).unwrap());

    // Parse the JSON file.
    let accounts: AccountMap = serde_json::from_reader(br).unwrap();
    info!(
      "  {:?} -> {} accounts",
      path.file_name().unwrap(),
      accounts.accounts.len()
    );

    for (mut addr, account) in accounts.accounts {
      let address = Address::from_str(&if addr.starts_with("0x") {
        addr.split_off(2)
      } else {
        addr
      }).unwrap();

      let mut account_state = AccountState {
        nonce: U256::from_dec_str(&account.nonce).unwrap(),
        address: address,
        balance: U256::from_dec_str(&account.balance).unwrap(),
        code: if account.code == "0x" {
          String::new()
        } else {
          account.code
        },
      };

      for (key, value) in account.storage {
        storage_request.push((
          address,
          H256::from_str(&key).unwrap(),
          H256::from_str(&value).unwrap(),
        ));
      }

      account_request.push(account_state);
    }
  }
  let result = client.inject_accounts(account_request).wait().unwrap();
  let result = client
    .inject_account_storage(storage_request)
    .wait()
    .unwrap();

  let init_state_request = InitStateRequest {};
  let result = client
    .init_genesis_block(init_state_request)
    .wait()
    .unwrap();
}