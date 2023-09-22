use candid::{Decode, Encode, Nat};
use ic_base_types::{CanisterId, PrincipalId};
use ic_canisters_http_types::{HttpRequest, HttpResponse};
use ic_icrc1_index_ng::{
    FeeCollectorRanges, GetAccountTransactionsArgs, GetAccountTransactionsResponse,
    GetAccountTransactionsResult, GetBlocksResponse, IndexArg, InitArg as IndexInitArg,
    ListSubaccountsArgs, Log, Status, TransactionWithId, DEFAULT_MAX_BLOCKS_PER_RESPONSE,
};
use ic_icrc1_ledger::{
    ChangeFeeCollector, InitArgsBuilder as LedgerInitArgsBuilder, LedgerArgument,
    UpgradeArgs as LedgerUpgradeArgs,
};
use ic_icrc1_test_utils::{valid_transactions_strategy, CallerTransferArg};
use ic_ledger_canister_core::archive::ArchiveOptions;
use ic_state_machine_tests::{StateMachine, WasmResult};
use icrc_ledger_types::icrc1::account::{Account, Subaccount};
use icrc_ledger_types::icrc1::transfer::{BlockIndex, TransferArg, TransferError};
use icrc_ledger_types::icrc3::blocks::{BlockRange, GenericBlock, GetBlocksRequest};
use icrc_ledger_types::icrc3::transactions::{Mint, Transaction, Transfer};
use num_traits::cast::ToPrimitive;
use proptest::test_runner::{Config as TestRunnerConfig, TestRunner};
use std::collections::HashSet;
use std::convert::TryInto;
use std::path::PathBuf;
use std::time::Duration;

const FEE: u64 = 10_000;
const ARCHIVE_TRIGGER_THRESHOLD: u64 = 10;
const NUM_BLOCKS_TO_ARCHIVE: usize = 10;
const MAX_BLOCKS_FROM_ARCHIVE: u64 = 10;

const MINTER: Account = Account {
    owner: PrincipalId::new(0, [0u8; 29]).0,
    subaccount: None,
};

// Metadata-related constants
const TOKEN_NAME: &str = "Test Token";
const TOKEN_SYMBOL: &str = "XTST";
const TEXT_META_KEY: &str = "test:image";
const TEXT_META_VALUE: &str = "grumpy_cat.png";
const BLOB_META_KEY: &str = "test:blob";
const BLOB_META_VALUE: &[u8] = b"\xca\xfe\xba\xbe";
const NAT_META_KEY: &str = "test:nat";
const NAT_META_VALUE: u128 = u128::MAX;
const INT_META_KEY: &str = "test:int";
const INT_META_VALUE: i128 = i128::MIN;

fn index_wasm() -> Vec<u8> {
    ic_test_utilities_load_wasm::load_wasm(
        std::env::var("CARGO_MANIFEST_DIR").unwrap(),
        "ic-icrc1-index",
        &[],
    )
}

fn index_ng_wasm() -> Vec<u8> {
    ic_test_utilities_load_wasm::load_wasm(
        std::env::var("CARGO_MANIFEST_DIR").unwrap(),
        "ic-icrc1-index-ng",
        &[],
    )
}

fn ledger_wasm() -> Vec<u8> {
    ic_test_utilities_load_wasm::load_wasm(
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join("ledger"),
        "ic-icrc1-ledger",
        &[],
    )
}

fn default_archive_options() -> ArchiveOptions {
    ArchiveOptions {
        trigger_threshold: ARCHIVE_TRIGGER_THRESHOLD as usize,
        num_blocks_to_archive: NUM_BLOCKS_TO_ARCHIVE,
        node_max_memory_size_bytes: None,
        max_message_size_bytes: None,
        controller_id: PrincipalId::new_user_test_id(100),
        cycles_for_archive_creation: None,
        max_transactions_per_response: Some(MAX_BLOCKS_FROM_ARCHIVE),
    }
}

fn install_ledger(
    env: &StateMachine,
    initial_balances: Vec<(Account, u64)>,
    archive_options: ArchiveOptions,
    fee_collector_account: Option<Account>,
) -> CanisterId {
    let mut builder = LedgerInitArgsBuilder::with_symbol_and_name(TOKEN_SYMBOL, TOKEN_NAME)
        .with_minting_account(MINTER)
        .with_transfer_fee(FEE)
        .with_metadata_entry(NAT_META_KEY, NAT_META_VALUE)
        .with_metadata_entry(INT_META_KEY, INT_META_VALUE)
        .with_metadata_entry(TEXT_META_KEY, TEXT_META_VALUE)
        .with_metadata_entry(BLOB_META_KEY, BLOB_META_VALUE)
        .with_archive_options(archive_options);
    if let Some(fee_collector_account) = fee_collector_account {
        builder = builder.with_fee_collector_account(fee_collector_account);
    }
    for (account, amount) in initial_balances {
        builder = builder.with_initial_balance(account, amount);
    }
    env.install_canister(
        ledger_wasm(),
        Encode!(&LedgerArgument::Init(builder.build())).unwrap(),
        None,
    )
    .unwrap()
}

fn upgrade_ledger(
    env: &StateMachine,
    ledger_id: CanisterId,
    fee_collector_account: Option<Account>,
) {
    let change_fee_collector =
        Some(fee_collector_account.map_or(ChangeFeeCollector::Unset, ChangeFeeCollector::SetTo));
    let args = LedgerArgument::Upgrade(Some(LedgerUpgradeArgs {
        metadata: None,
        token_name: None,
        token_symbol: None,
        transfer_fee: None,
        change_fee_collector,
        max_memo_length: None,
        feature_flags: None,
        maximum_number_of_accounts: None,
        accounts_overflow_trim_quantity: None,
    }));
    env.upgrade_canister(ledger_id, ledger_wasm(), Encode!(&args).unwrap())
        .unwrap()
}

fn install_index_ng(env: &StateMachine, ledger_id: CanisterId) -> CanisterId {
    let args = IndexArg::Init(IndexInitArg {
        ledger_id: ledger_id.into(),
    });
    env.install_canister(index_ng_wasm(), Encode!(&args).unwrap(), None)
        .unwrap()
}

fn install_index(env: &StateMachine, ledger_id: CanisterId) -> CanisterId {
    let args = ic_icrc1_index::InitArgs { ledger_id };
    env.install_canister(index_wasm(), Encode!(&args).unwrap(), None)
        .unwrap()
}

fn account(owner: u64, subaccount: u128) -> Account {
    let mut sub: [u8; 32] = [0; 32];
    sub[..16].copy_from_slice(&subaccount.to_be_bytes());
    Account {
        owner: PrincipalId::new_user_test_id(owner).0,
        subaccount: Some(sub),
    }
}

fn status(env: &StateMachine, index_id: CanisterId) -> Status {
    let res = env
        .query(index_id, "status", Encode!(&()).unwrap())
        .expect("Failed to send status")
        .bytes();
    Decode!(&res, Status).expect("Failed to decode status response")
}

fn assert_reply(result: WasmResult) -> Vec<u8> {
    match result {
        WasmResult::Reply(bytes) => bytes,
        WasmResult::Reject(reject) => {
            panic!("Expected a successful reply, got a reject: {}", reject)
        }
    }
}

fn get_logs(env: &StateMachine, index_id: CanisterId) -> Log {
    let request = HttpRequest {
        method: "".to_string(),
        url: "/logs".to_string(),
        headers: vec![],
        body: serde_bytes::ByteBuf::new(),
    };
    let response = Decode!(
        &assert_reply(
            env.execute_ingress(index_id, "http_request", Encode!(&request).unwrap(),)
                .expect("failed to get index-ng info")
        ),
        HttpResponse
    )
    .unwrap();
    serde_json::from_slice(&response.body).expect("failed to parse index-ng log")
}

// Helper function that calls tick on env until either
// the index canister has synced all the blocks up to the
// last one in the ledger or enough attempts passed and therefore
// it fails
fn wait_until_sync_is_completed(env: &StateMachine, index_id: CanisterId, ledger_id: CanisterId) {
    const MAX_ATTEMPTS: u8 = 100; // no reason for this number
    let mut num_blocks_synced = u64::MAX;
    let mut chain_length = u64::MAX;
    for _i in 0..MAX_ATTEMPTS {
        env.advance_time(Duration::from_secs(60));
        env.tick();
        num_blocks_synced = status(env, index_id).num_blocks_synced.0.to_u64().unwrap();
        chain_length = ledger_get_all_blocks(env, ledger_id, 0, 1).chain_length;
        if num_blocks_synced == chain_length {
            return;
        }
    }
    let log = get_logs(env, index_id);
    let mut log_lines = String::new();
    for entry in log.entries {
        log_lines.push_str(&format!(
            "{} {}:{} {}\n",
            entry.timestamp, entry.file, entry.line, entry.message
        ));
    }
    panic!("The index canister was unable to sync all the blocks with the ledger. Number of blocks synced {} but the Ledger chain length is {}.\nLogs:\n{}", num_blocks_synced, chain_length, log_lines);
}

fn icrc1_balance_of(env: &StateMachine, canister_id: CanisterId, account: Account) -> u64 {
    let res = env
        .execute_ingress(canister_id, "icrc1_balance_of", Encode!(&account).unwrap())
        .expect("Failed to send icrc1_balance_of")
        .bytes();
    Decode!(&res, Nat)
        .expect("Failed to decode icrc1_balance_of response")
        .0
        .to_u64()
        .expect("Balance must be a u64!")
}

// Retrieves blocks from the Ledger and the Archives
fn ledger_get_all_blocks(
    env: &StateMachine,
    ledger_id: CanisterId,
    start: u64,
    length: u64,
) -> icrc_ledger_types::icrc3::blocks::GetBlocksResponse {
    let req = GetBlocksRequest {
        start: start.into(),
        length: length.into(),
    };
    let req = Encode!(&req).expect("Failed to encode GetBlocksRequest");
    let res = env
        .query(ledger_id, "get_blocks", req)
        .expect("Failed to send get_blocks request")
        .bytes();
    let res = Decode!(&res, icrc_ledger_types::icrc3::blocks::GetBlocksResponse)
        .expect("Failed to decode GetBlocksResponse");
    let mut blocks = vec![];
    for archived in &res.archived_blocks {
        // Archive nodes paginate their results. We need to fetch all the
        // blocks and therefore we loop until all of them
        // have been fetched.
        let mut curr_start = archived.start.clone();
        while curr_start < archived.length {
            let req = GetBlocksRequest {
                start: curr_start.clone(),
                length: archived.length.clone() - (curr_start.clone() - archived.start.clone()),
            };
            let req = Encode!(&req).expect("Failed to encode GetBlocksRequest for archive node");
            let canister_id = archived.callback.canister_id.as_ref().try_into().unwrap();
            let res = env
                .query(canister_id, archived.callback.method.clone(), req)
                .expect("Failed to send get_blocks request to archive")
                .bytes();
            let res = Decode!(&res, BlockRange)
                .expect("Failed to decode get_blocks response for archive node")
                .blocks;
            assert!(!res.is_empty());
            curr_start += res.len();
            blocks.extend(res);
        }
    }
    blocks.extend(res.blocks);
    icrc_ledger_types::icrc3::blocks::GetBlocksResponse { blocks, ..res }
}

fn get_blocks(
    env: &StateMachine,
    index_id: CanisterId,
    start: u64,
    length: u64,
) -> GetBlocksResponse {
    let req = GetBlocksRequest {
        start: start.into(),
        length: length.into(),
    };
    let req = Encode!(&req).expect("Failed to encode GetBlocksRequest");
    let res = env
        .execute_ingress(index_id, "get_blocks", req)
        .expect("Failed to send get_blocks request")
        .bytes();
    Decode!(&res, GetBlocksResponse).expect("Failed to decode GetBlocksResponse")
}

// Returns all blocks in the index by iterating over the pages
fn index_get_all_blocks(
    env: &StateMachine,
    index_id: CanisterId,
    start: u64,
    length: u64,
) -> Vec<GenericBlock> {
    let length = length.min(get_blocks(env, index_id, 0, 0).chain_length);
    let mut res = vec![];
    let mut curr_start = start;
    while length > res.len() as u64 {
        let blocks = get_blocks(env, index_id, curr_start, length - (curr_start - start)).blocks;
        assert!(!blocks.is_empty());
        curr_start += blocks.len() as u64;
        res.extend(blocks);
    }
    res
}

fn icrc1_transfer(
    env: &StateMachine,
    ledger_id: CanisterId,
    caller: PrincipalId,
    arg: TransferArg,
) -> BlockIndex {
    let req = Encode!(&arg).expect("Failed to encode TransferArg");
    let res = env
        .execute_ingress_as(caller, ledger_id, "icrc1_transfer", req)
        .expect("Failed to transfer tokens")
        .bytes();
    Decode!(&res, Result<BlockIndex, TransferError>)
        .expect("Failed to decode Result<BlockIndex, TransferError>")
        .expect("Failed to transfer tokens")
}

fn transfer(
    env: &StateMachine,
    ledger_id: CanisterId,
    from: Account,
    to: Account,
    amount: u64,
) -> BlockIndex {
    let Account { owner, subaccount } = from;
    let req = TransferArg {
        from_subaccount: subaccount,
        to,
        amount: amount.into(),
        created_at_time: None,
        fee: None,
        memo: None,
    };
    icrc1_transfer(env, ledger_id, owner.into(), req)
}

// Same as get_account_transactions but with the old index interface
fn old_get_account_transactions(
    env: &StateMachine,
    index_id: CanisterId,
    account: Account,
    start: Option<u64>,
    max_results: u64,
) -> ic_icrc1_index::GetTransactionsResult {
    let req = ic_icrc1_index::GetAccountTransactionsArgs {
        account,
        start: start.map(|n| n.into()),
        max_results: max_results.into(),
    };
    let req = Encode!(&req).expect("Failed to encode GetAccountTransactionsArgs");
    let res = env
        .execute_ingress(index_id, "get_account_transactions", req)
        .expect("Failed to get_account_transactions")
        .bytes();
    Decode!(&res, ic_icrc1_index::GetTransactionsResult)
        .expect("Failed to decode GetTransactionsResult")
}

fn get_account_transactions(
    env: &StateMachine,
    index_id: CanisterId,
    account: Account,
    start: Option<u64>,
    max_results: u64,
) -> GetAccountTransactionsResponse {
    let req = GetAccountTransactionsArgs {
        account,
        start: start.map(|n| n.into()),
        max_results: max_results.into(),
    };
    let req = Encode!(&req).expect("Failed to encode GetAccountTransactionsArgs");
    let res = env
        .execute_ingress(index_id, "get_account_transactions", req)
        .expect("Failed to get_account_transactions")
        .bytes();
    Decode!(&res, GetAccountTransactionsResult)
        .expect("Failed to decode GetAccountTransactionsArgs")
        .expect("Failed to perform GetAccountTransactionsArgs")
}

fn list_subaccounts(
    env: &StateMachine,
    index: CanisterId,
    principal: PrincipalId,
    start: Option<Subaccount>,
) -> Vec<Subaccount> {
    Decode!(
        &env.execute_ingress_as(
            principal,
            index,
            "list_subaccounts",
            Encode!(&ListSubaccountsArgs {
                owner: principal.into(),
                start,
            })
            .unwrap()
        )
        .expect("failed to list_subaccounts")
        .bytes(),
        Vec<Subaccount>
    )
    .expect("failed to decode list_subaccounts response")
}

fn get_fee_collectors_ranges(env: &StateMachine, index: CanisterId) -> FeeCollectorRanges {
    Decode!(
        &env.execute_ingress(index, "get_fee_collectors_ranges", Encode!(&()).unwrap())
            .expect("failed to get_fee_collectors_ranges")
            .bytes(),
        FeeCollectorRanges
    )
    .expect("failed to decode get_fee_collectors_ranges response")
}

// Assert that the index canister contains the same blocks as the ledger
#[track_caller]
fn assert_ledger_index_parity(env: &StateMachine, ledger_id: CanisterId, index_id: CanisterId) {
    let ledger_blocks = ledger_get_all_blocks(env, ledger_id, 0, u64::MAX).blocks;
    let index_blocks = index_get_all_blocks(env, index_id, 0, u64::MAX);
    assert_eq!(ledger_blocks.len(), index_blocks.len());

    for idx in 0..ledger_blocks.len() {
        assert_eq!(
            ledger_blocks[idx], index_blocks[idx],
            "block_index: {}",
            idx
        );
    }
}

#[test]
fn test_ledger_growing() {
    // check that the index canister can incrementally get the blocks from the ledger.

    let initial_balances: Vec<_> = vec![(account(1, 0), 1_000_000_000_000)];
    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, initial_balances, default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);

    // test initial mint block
    wait_until_sync_is_completed(env, index_id, ledger_id);
    assert_ledger_index_parity(env, ledger_id, index_id);

    // test first transfer block
    transfer(env, ledger_id, account(1, 0), account(2, 0), 1);
    wait_until_sync_is_completed(env, index_id, ledger_id);
    assert_ledger_index_parity(env, ledger_id, index_id);

    // test multiple blocks
    for (from, to, amount) in [
        (account(1, 0), account(1, 1), 1_000_000),
        (account(1, 0), account(2, 0), 1_000_001),
        (account(1, 1), account(2, 0), 1),
    ] {
        transfer(env, ledger_id, from, to, amount);
    }
    wait_until_sync_is_completed(env, index_id, ledger_id);
    assert_ledger_index_parity(env, ledger_id, index_id);

    // test archived blocks
    for _i in 0..(ARCHIVE_TRIGGER_THRESHOLD as usize + 1) {
        transfer(env, ledger_id, account(1, 0), account(1, 2), 1);
    }
    wait_until_sync_is_completed(env, index_id, ledger_id);
    assert_ledger_index_parity(env, ledger_id, index_id);
}

#[test]
fn test_archive_indexing() {
    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, vec![], default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);

    // test indexing archive by forcing the ledger to archive some transactions
    // and by having enough transactions such that the index must use archive
    // pagination
    for i in 0..(ARCHIVE_TRIGGER_THRESHOLD + MAX_BLOCKS_FROM_ARCHIVE * 4) {
        transfer(env, ledger_id, MINTER, account(i, 0), i * 1_000_000);
    }

    wait_until_sync_is_completed(env, index_id, ledger_id);
    assert_ledger_index_parity(env, ledger_id, index_id);
}

#[track_caller]
fn assert_tx_eq(tx1: &Transaction, tx2: &Transaction) {
    if let Some(burn1) = &tx1.burn {
        let burn2 = tx2.burn.as_ref().unwrap();
        assert_eq!(burn1.amount, burn2.amount, "amount");
        assert_eq!(burn1.from, burn2.from, "from");
        assert_eq!(burn1.memo, burn2.memo, "memo");
    } else if let Some(mint1) = &tx1.mint {
        let mint2 = tx2.mint.as_ref().unwrap();
        assert_eq!(mint1.amount, mint2.amount, "amount");
        assert_eq!(mint1.memo, mint2.memo, "memo");
        assert_eq!(mint1.to, mint2.to, "to");
    } else if let Some(transfer1) = &tx1.transfer {
        let transfer2 = tx2.transfer.as_ref().unwrap();
        assert_eq!(transfer1.amount, transfer2.amount, "amount");
        assert_eq!(transfer1.fee, transfer2.fee, "fee");
        assert_eq!(transfer1.from, transfer2.from, "from");
        assert_eq!(transfer1.memo, transfer2.memo, "memo");
        assert_eq!(transfer1.to, transfer2.to, "to");
    } else {
        panic!("Something is wrong with tx1: {:?}", tx1);
    }
}

// checks that two txs are equal minus the fields set by the ledger (e.g. timestamp)
#[track_caller]
fn assert_tx_with_id_eq(tx1: &TransactionWithId, tx2: &TransactionWithId) {
    assert_eq!(tx1.id, tx2.id, "id");
    assert_tx_eq(&tx1.transaction, &tx2.transaction);
}

#[track_caller]
fn assert_txs_with_id_eq(txs1: Vec<TransactionWithId>, txs2: Vec<TransactionWithId>) {
    assert_eq!(
        txs1.len(),
        txs2.len(),
        "Different number of transactions!\ntxs1: {:?}\ntxs2: {:?}",
        txs1.iter().map(|tx| tx.id.clone()).collect::<Vec<Nat>>(),
        txs2.iter().map(|tx| tx.id.clone()).collect::<Vec<Nat>>()
    );
    for i in 0..txs1.len() {
        assert_tx_with_id_eq(&txs1[i], &txs2[i]);
    }
}

#[test]
fn test_get_account_transactions() {
    let initial_balances: Vec<_> = vec![(account(1, 0), 1_000_000_000_000)];
    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, initial_balances, default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);

    // List of the transactions that the test is going to add. This exists to make
    // the test easier to read
    let tx0 = TransactionWithId {
        id: 0.into(),
        transaction: Transaction::mint(
            Mint {
                to: account(1, 0),
                amount: 1_000_000_000_000_u64.into(),
                created_at_time: None,
                memo: None,
            },
            0,
        ),
    };
    let tx1 = TransactionWithId {
        id: 1.into(),
        transaction: Transaction::transfer(
            Transfer {
                from: account(1, 0),
                to: account(2, 0),
                spender: None,
                amount: 1_000_000.into(),
                fee: None,
                created_at_time: None,
                memo: None,
            },
            0,
        ),
    };
    let tx2 = TransactionWithId {
        id: 2.into(),
        transaction: Transaction::transfer(
            Transfer {
                from: account(1, 0),
                to: account(2, 0),
                spender: None,
                amount: 2_000_000.into(),
                fee: None,
                created_at_time: None,
                memo: None,
            },
            0,
        ),
    };
    let tx3 = TransactionWithId {
        id: 3.into(),
        transaction: Transaction::transfer(
            Transfer {
                from: account(2, 0),
                to: account(1, 1),
                spender: None,
                amount: 1_000_000.into(),
                fee: None,
                created_at_time: None,
                memo: None,
            },
            0,
        ),
    };

    ////////////
    //// phase 1: only 1 mint to (1, 0)
    wait_until_sync_is_completed(env, index_id, ledger_id);

    // account (1, 0) has one mint
    let actual_txs =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![tx0.clone()]);

    // account (2, 0) has no transactions
    let actual_txs =
        get_account_transactions(env, index_id, account(2, 0), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![]);

    /////////////
    //// phase 2: transfer from (1, 0) to (2, 0)
    transfer(env, ledger_id, account(1, 0), account(2, 0), 1_000_000);
    wait_until_sync_is_completed(env, index_id, ledger_id);

    // account (1, 0) has one transfer and one mint
    let actual_txs =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![tx1.clone(), tx0.clone()]);

    // account (2, 0) has one transfer only
    let actual_txs =
        get_account_transactions(env, index_id, account(2, 0), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![tx1.clone()]);

    // account (3, 0), (1, 1) and (2, 1) have no transactions
    for account in [account(3, 0), account(1, 1), account(2, 1)] {
        let actual_txs =
            get_account_transactions(env, index_id, account, None, u64::MAX).transactions;
        assert_txs_with_id_eq(actual_txs, vec![]);
    }

    ////////////
    //// phase 3: transfer from (1, 0) to (2, 0)
    ////          transfer from (2, 0) to (1, 1)
    transfer(env, ledger_id, account(1, 0), account(2, 0), 2_000_000);
    transfer(env, ledger_id, account(2, 0), account(1, 1), 1_000_000);
    wait_until_sync_is_completed(env, index_id, ledger_id);

    // account (1, 0) has two transfers and one mint
    let actual_txs =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).transactions;
    let expected_txs = vec![tx2.clone(), tx1.clone(), tx0];
    assert_txs_with_id_eq(actual_txs, expected_txs);

    // account (2, 0) has three transfers
    let actual_txs =
        get_account_transactions(env, index_id, account(2, 0), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![tx3.clone(), tx2, tx1]);

    // account (1, 1) has one transfer
    let actual_txs =
        get_account_transactions(env, index_id, account(1, 1), None, u64::MAX).transactions;
    assert_txs_with_id_eq(actual_txs, vec![tx3]);
}

#[test]
fn test_get_account_transactions_start_length() {
    // 10 mint transactions to index for the same account
    let initial_balances: Vec<_> = (0..10).map(|i| (account(1, 0), i * 10_000)).collect();
    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, initial_balances, default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);
    let expected_txs: Vec<_> = (0..10)
        .map(|i| TransactionWithId {
            id: i.into(),
            transaction: Transaction::mint(
                Mint {
                    to: account(1, 0),
                    amount: (i * 10_000).into(),
                    created_at_time: None,
                    memo: None,
                },
                0,
            ),
        })
        .collect();

    wait_until_sync_is_completed(env, index_id, ledger_id);

    // get the most n recent transaction with start set to none
    for n in 1..10 {
        let actual_txs =
            get_account_transactions(env, index_id, account(1, 0), None, n).transactions;
        let expected_txs: Vec<_> = (0..10)
            .rev()
            .take(n as usize)
            .map(|i| expected_txs[i as usize].clone())
            .collect();
        assert_txs_with_id_eq(actual_txs, expected_txs.clone());
    }

    // get the most n recent transaction with start set to some index
    for start in 0..=10 {
        for n in 1..(10 - start) {
            let expected_txs: Vec<_> = (0..start)
                .rev()
                .take(n as usize)
                .map(|i| expected_txs[i as usize].clone())
                .collect();
            let actual_txs =
                get_account_transactions(env, index_id, account(1, 0), Some(start), n).transactions;
            assert_txs_with_id_eq(actual_txs, expected_txs);
        }
    }
}

#[test]
fn test_get_account_transactions_pagination() {
    // 10_000 mint transactions to index for the same account
    let initial_balances: Vec<_> = (0..10_000).map(|i| (account(1, 0), i * 10_000)).collect();
    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, initial_balances, default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);

    wait_until_sync_is_completed(env, index_id, ledger_id);

    // The index get_account_transactions endpoint returns batches of transactions
    // in descending order of index, i.e. the first index returned in the result
    // is the biggest id in the result while the last index is the lowest.
    // The start parameter of the function is the last seen index and the result
    // will contain the next batch of indexes after that one.

    let mut start = None; // the start id of the next batch request

    // if start == Some(0) then we can stop as there is no index that is smaller
    // than 0.
    while start != Some(0) {
        let res = get_account_transactions(env, index_id, account(1, 0), start, u64::MAX);

        // if the batch is empty then get_account_transactions
        // didn't return the expected batch for the given start
        if res.transactions.is_empty() {
            panic!(
                "get_account_transactions({:?}, u64::MAX) returned an empty batch!",
                start
            );
        }

        let mut last_seen_txid = start;
        for TransactionWithId { id, transaction } in &res.transactions {
            let id = id.0.to_u64().unwrap();

            // transactions ids must be unique and in descending order
            if let Some(last_seen_txid) = last_seen_txid {
                assert!(id < last_seen_txid);
            }
            last_seen_txid = Some(id);

            // check the transaction itself
            assert_tx_eq(
                &Transaction {
                    kind: "mint".into(),
                    burn: None,
                    mint: Some(Mint {
                        to: account(1, 0),
                        amount: (id * 10_000).into(),
                        created_at_time: None,
                        memo: None,
                    }),
                    transfer: None,
                    approve: None,
                    timestamp: 0,
                },
                transaction,
            );
        }

        // !res.transactions.is_empty() and the check on descending
        // order guarantee that last_seen_txid < start
        start = last_seen_txid;
    }
}

#[test]
fn test_icrc1_balance_of() {
    // 1 case only because the test is expensive to run
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(1));
    runner
        .run(
            &(valid_transactions_strategy(MINTER, FEE, 100),),
            |(transactions,)| {
                let env = &StateMachine::new();
                let ledger_id = install_ledger(env, vec![], default_archive_options(), None);
                let index_id = install_index_ng(env, ledger_id);

                for CallerTransferArg {
                    caller,
                    transfer_arg,
                } in &transactions
                {
                    icrc1_transfer(env, ledger_id, PrincipalId(*caller), transfer_arg.clone());
                }
                wait_until_sync_is_completed(env, index_id, ledger_id);

                for account in transactions
                    .iter()
                    .flat_map(|tx| tx.accounts())
                    .collect::<HashSet<Account>>()
                {
                    assert_eq!(
                        icrc1_balance_of(env, ledger_id, account),
                        icrc1_balance_of(env, index_id, account)
                    );
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn test_list_subaccounts() {
    // For this test, we add minting operations for some principals:
    // - The principal 1 has one account with the last possible
    // subaccount.
    // - The principal 2 has a number of subaccounts equals to
    // two times the DEFAULT_MAX_BLOCKS_PER_RESPONSE. Therefore fetching
    // its subaccounts will trigger pagination.
    // - The principal 3 has one account with the first possible
    // subaccount.
    // - The principal 4 has one account with the default subaccount,
    // which should map to [0;32] in the index.

    let account_1 = Account {
        owner: PrincipalId::new_user_test_id(1).into(),
        subaccount: Some([u8::MAX; 32]),
    };
    let accounts_2: Vec<_> = (0..(DEFAULT_MAX_BLOCKS_PER_RESPONSE * 2))
        .map(|i| account(2, i as u128))
        .collect();
    let account_3 = account(3, 0);
    let account_4 = Account {
        owner: PrincipalId::new_user_test_id(4).into(),
        subaccount: None,
    };

    let mut initial_balances: Vec<_> = vec![
        (account_1, 10_000),
        (account_3, 10_000),
        (account_4, 40_000),
    ];
    initial_balances.extend(accounts_2.iter().map(|account| (*account, 10_000)));

    let env = &StateMachine::new();
    let ledger_id = install_ledger(env, initial_balances, default_archive_options(), None);
    let index_id = install_index_ng(env, ledger_id);

    wait_until_sync_is_completed(env, index_id, ledger_id);

    // list account_1.owner subaccounts when no starting subaccount is specified
    assert_eq!(
        vec![*account_1.effective_subaccount()],
        list_subaccounts(env, index_id, PrincipalId(account_1.owner), None)
    );

    // list account_3.owner subaccounts when no starting subaccount is specified
    assert_eq!(
        vec![*account_3.effective_subaccount()],
        list_subaccounts(env, index_id, PrincipalId(account_3.owner), None)
    );

    // list account_3.owner subaccounts when an existing starting subaccount is specified but no subaccount is in that range
    assert!(list_subaccounts(
        env,
        index_id,
        PrincipalId(account_3.owner),
        Some(*account(3, 1).effective_subaccount())
    )
    .is_empty());

    // list acccount_4.owner subaccounts should return the default subaccount
    // mapped to [0;32]
    assert_eq!(
        vec![[0; 32]],
        list_subaccounts(env, index_id, PrincipalId(account_4.owner), None)
    );

    // account_2.owner should have two batches of subaccounts
    let principal_2 = accounts_2.get(0).unwrap().owner;
    let batch_1 = list_subaccounts(env, index_id, PrincipalId(principal_2), None);
    let expected_batch_1: Vec<_> = accounts_2
        .iter()
        .take(DEFAULT_MAX_BLOCKS_PER_RESPONSE as usize)
        .map(|account| *account.effective_subaccount())
        .collect();
    assert_eq!(expected_batch_1, batch_1);

    let batch_2 = list_subaccounts(
        env,
        index_id,
        PrincipalId(principal_2),
        Some(*batch_1.last().unwrap()),
    );
    let expected_batch_2: Vec<_> = accounts_2
        .iter()
        .skip(DEFAULT_MAX_BLOCKS_PER_RESPONSE as usize)
        .take(DEFAULT_MAX_BLOCKS_PER_RESPONSE as usize)
        .map(|account| *account.effective_subaccount())
        .collect();
    assert_eq!(expected_batch_2, batch_2);
}

#[test]
fn test_post_upgrade_start_timer() {
    let env = &StateMachine::new();
    let ledger_id = install_ledger(
        env,
        vec![(account(1, 0), 10_000_000)],
        default_archive_options(),
        None,
    );
    let index_id = install_index_ng(env, ledger_id);

    wait_until_sync_is_completed(env, index_id, ledger_id);

    env.upgrade_canister(index_id, index_ng_wasm(), vec![])
        .unwrap();

    // check that the index syncs the new block (wait_until_sync_is_completed fails
    // if the new block is not synced).
    transfer(env, ledger_id, account(1, 0), account(2, 0), 2_000_000);
    wait_until_sync_is_completed(env, index_id, ledger_id);
}

#[test]
fn test_oldest_tx_id() {
    let env = &StateMachine::new();
    let ledger_id = install_ledger(
        env,
        vec![(account(1, 0), 10_000_000)],
        default_archive_options(),
        None,
    );
    let index_id = install_index_ng(env, ledger_id);

    // account(2, 0) and account(3, 0) have no transactions so oldest_tx_id should be None
    for account in [account(2, 0), account(3, 0)] {
        let oldest_tx_id =
            get_account_transactions(env, index_id, account, None, u64::MAX).oldest_tx_id;
        assert_eq!(None, oldest_tx_id);
    }

    // account(1, 0) oldest_tx_id is 0, i.e. the mint at ledger init
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(0.into()), oldest_tx_id);

    ////
    // add one block for account(1, 0) and account(2, 0)
    transfer(env, ledger_id, account(1, 0), account(2, 0), 1_000_000);
    wait_until_sync_is_completed(env, index_id, ledger_id);

    // account(1, 0) oldest_tx_id is still 0
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(0.into()), oldest_tx_id);

    // account(2, 0) oldest_tx_id is 1, i.e. the new transfer
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(2, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(1.into()), oldest_tx_id);

    // account(3, 0) oldest_tx_id is still None
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(3, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(None, oldest_tx_id);

    ////
    // add one block for account(1, 0) and account(2, 0)
    // add the first block for account(3, 0)
    transfer(env, ledger_id, account(1, 0), account(2, 0), 2_000_000);
    transfer(env, ledger_id, account(1, 0), account(3, 0), 3_000_000);
    wait_until_sync_is_completed(env, index_id, ledger_id);

    // account(1, 0) oldest_tx_id is still 0
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(1, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(0.into()), oldest_tx_id);

    // account(2, 0) oldest_tx_id is still 1
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(2, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(1.into()), oldest_tx_id);

    // account(3, 0) oldest_tx_id is 3, i.e. the last block index
    let oldest_tx_id =
        get_account_transactions(env, index_id, account(3, 0), None, u64::MAX).oldest_tx_id;
    assert_eq!(Some(3.into()), oldest_tx_id);

    // there should be no fee collector
    assert_eq!(get_fee_collectors_ranges(env, index_id).ranges, vec![]);
}

#[test]
fn test_fee_collector() {
    let env = &StateMachine::new();
    let fee_collector = account(42, 0);
    let ledger_id = install_ledger(
        env,
        vec![(account(1, 0), 10_000_000)], // txid: 0
        default_archive_options(),
        Some(fee_collector),
    );
    let index_id = install_index_ng(env, ledger_id);

    assert_eq!(
        icrc1_balance_of(env, ledger_id, fee_collector),
        icrc1_balance_of(env, index_id, fee_collector)
    );

    transfer(env, ledger_id, account(1, 0), account(2, 0), 100_000); // txid: 1
    transfer(env, ledger_id, account(1, 0), account(3, 0), 200_000); // txid: 2
    transfer(env, ledger_id, account(1, 0), account(2, 0), 300_000); // txid: 3

    wait_until_sync_is_completed(env, index_id, ledger_id);

    assert_eq!(
        icrc1_balance_of(env, ledger_id, fee_collector),
        icrc1_balance_of(env, index_id, fee_collector)
    );

    assert_eq!(
        get_fee_collectors_ranges(env, index_id).ranges,
        vec![(fee_collector, vec![(0.into(), 4.into())])]
    );

    // remove the fee collector to burn some transactions fees
    upgrade_ledger(env, ledger_id, None);

    transfer(env, ledger_id, account(1, 0), account(2, 0), 400_000); // txid: 4
    transfer(env, ledger_id, account(1, 0), account(2, 0), 500_000); // txid: 5

    wait_until_sync_is_completed(env, index_id, ledger_id);

    assert_eq!(
        icrc1_balance_of(env, ledger_id, fee_collector),
        icrc1_balance_of(env, index_id, fee_collector)
    );

    assert_eq!(
        get_fee_collectors_ranges(env, index_id).ranges,
        vec![(fee_collector, vec![(0.into(), 4.into())])]
    );

    // add a new fee collector different from the first one
    let new_fee_collector = account(42, 42);
    upgrade_ledger(env, ledger_id, Some(new_fee_collector));

    transfer(env, ledger_id, account(1, 0), account(2, 0), 400_000); // txid: 6

    wait_until_sync_is_completed(env, index_id, ledger_id);

    for fee_collector in &[fee_collector, new_fee_collector] {
        assert_eq!(
            icrc1_balance_of(env, ledger_id, *fee_collector),
            icrc1_balance_of(env, index_id, *fee_collector)
        );
    }

    assert_eq!(
        get_fee_collectors_ranges(env, index_id).ranges,
        vec![
            (fee_collector, vec![(0.into(), 4.into())]),
            (new_fee_collector, vec![(6.into(), 7.into())])
        ]
    );

    // add back the original fee_collector and make a couple of transactions again
    upgrade_ledger(env, ledger_id, Some(fee_collector));

    transfer(env, ledger_id, account(1, 0), account(2, 0), 400_000); // txid: 7
    transfer(env, ledger_id, account(1, 0), account(2, 0), 400_000); // txid: 8

    wait_until_sync_is_completed(env, index_id, ledger_id);

    for fee_collector in &[fee_collector, new_fee_collector] {
        assert_eq!(
            icrc1_balance_of(env, ledger_id, *fee_collector),
            icrc1_balance_of(env, index_id, *fee_collector)
        );
    }

    assert_eq!(
        get_fee_collectors_ranges(env, index_id).ranges,
        vec![
            (
                fee_collector,
                vec![(0.into(), 4.into()), (7.into(), 9.into())]
            ),
            (new_fee_collector, vec![(6.into(), 7.into())])
        ]
    );
}

#[test]
fn test_get_account_transactions_vs_old_index() {
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(1));
    runner
        .run(
            &(valid_transactions_strategy(MINTER, FEE, 100),),
            |(transactions,)| {
                let env = &StateMachine::new();
                let ledger_id = install_ledger(env, vec![], default_archive_options(), None);
                let index_ng_id = install_index_ng(env, ledger_id);
                let index_id = install_index(env, ledger_id);

                for CallerTransferArg {
                    caller,
                    transfer_arg,
                } in &transactions
                {
                    icrc1_transfer(env, ledger_id, PrincipalId(*caller), transfer_arg.clone());
                }
                wait_until_sync_is_completed(env, index_ng_id, ledger_id);

                for account in transactions
                    .iter()
                    .flat_map(|tx| tx.accounts())
                    .collect::<HashSet<Account>>()
                {
                    assert_eq!(
                        old_get_account_transactions(env, index_id, account, None, u64::MAX),
                        old_get_account_transactions(env, index_ng_id, account, None, u64::MAX),
                    );
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn test_upgrade_index_to_index_ng() {
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(1));
    runner
        .run(
            &(valid_transactions_strategy(MINTER, FEE, 100),),
            |(transactions,)| {
                let env = &StateMachine::new();
                let ledger_id = install_ledger(env, vec![], default_archive_options(), None);
                let index_ng_id = install_index_ng(env, ledger_id);
                let index_id = install_index(env, ledger_id);

                for CallerTransferArg {
                    caller,
                    transfer_arg,
                } in &transactions
                {
                    icrc1_transfer(env, ledger_id, PrincipalId(*caller), transfer_arg.clone());
                }

                env.tick();
                wait_until_sync_is_completed(env, index_ng_id, ledger_id);

                // upgrade the index canister to the index-ng
                let arg = IndexArg::Init(IndexInitArg {
                    ledger_id: ledger_id.into(),
                });
                let arg = Encode!(&arg).unwrap();
                env.upgrade_canister(index_id, index_ng_wasm(), arg)
                    .unwrap();

                wait_until_sync_is_completed(env, index_id, ledger_id);

                // check that the old get_account_transactions still works and return
                // the right data
                for account in transactions
                    .iter()
                    .flat_map(|tx| tx.accounts())
                    .collect::<HashSet<Account>>()
                {
                    assert_eq!(
                        old_get_account_transactions(env, index_id, account, None, u64::MAX),
                        old_get_account_transactions(env, index_ng_id, account, None, u64::MAX),
                    );
                }
                Ok(())
            },
        )
        .unwrap();
}
