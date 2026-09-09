//! Suite — vault detail event indexing tests (issue #675).
//!
//! Acceptance criteria exercised here:
//!
//! - `adapter_allocation`: VaultAllocated (Allocated), VaultPulled (Pulled), and
//!   VaultRebalanced (Rebalanced) events are written to `adapter_allocations`.
//!
//! - `fee_charged`: ExitFeeCharged events are written to `vault_fee_events`.
//!
//! - ERC-4626 Deposit and Withdraw events are written to `vault_transfer_events`.
//!
//! All tests skip cleanly when Docker is not available.
//!
//! Canonical: docs/architecture.md §5.4

mod common;

use alloy_primitives::{Address, U256};
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{
    abi::IVaultEvents,
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn stub_block(number: u64, hash_byte: u8, parent_byte: u8) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       format!("0x{}", hex::encode([hash_byte; 32])),
        "parentHash": format!("0x{}", hex::encode([parent_byte; 32])),
        "timestamp":  "0x65000000",
        "transactions": []
    })
}

/// Encode an `Allocated(uint256 indexed index, address indexed adapter, uint256 amount)` log.
fn encode_allocated_log(
    vault_addr: Address,
    adapter: Address,
    index: U256,
    amount: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::Allocated {
        index,
        adapter,
        amount,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `Pulled(uint256 indexed index, address indexed adapter, uint256 amount)` log.
fn encode_pulled_log(
    vault_addr: Address,
    adapter: Address,
    index: U256,
    amount: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::Pulled {
        index,
        adapter,
        amount,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `Rebalanced(uint256 totalMoved)` log.
fn encode_rebalanced_log(
    vault_addr: Address,
    total_moved: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::Rebalanced {
        totalMoved: total_moved,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode an `ExitFeeCharged(address indexed owner, address indexed receiver,
/// uint256 grossAssets, uint256 fee, uint256 netAssets)` log.
#[allow(clippy::too_many_arguments)]
fn encode_exit_fee_charged_log(
    vault_addr: Address,
    owner: Address,
    receiver: Address,
    gross_assets: U256,
    fee: U256,
    net_assets: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::ExitFeeCharged {
        owner,
        receiver,
        grossAssets: gross_assets,
        fee,
        netAssets: net_assets,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode an ERC-4626 `Deposit(address indexed caller, address indexed owner,
/// uint256 assets, uint256 shares)` log.
#[allow(clippy::too_many_arguments)]
fn encode_erc4626_deposit_log(
    vault_addr: Address,
    caller: Address,
    owner: Address,
    assets: U256,
    shares: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::Deposit {
        caller,
        owner,
        assets,
        shares,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_erc4626_withdraw_log(
    vault_addr: Address,
    caller: Address,
    receiver: Address,
    owner: Address,
    assets: U256,
    shares: U256,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultEvents::Withdraw {
        caller,
        receiver,
        owner,
        assets,
        shares,
    };
    let log_data: LogData = event.encode_log_data();
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", vault_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

fn base_config(vault: Address) -> IndexerConfig {
    IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xCCu8; 20]),
        vault,
        registry: None,
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 0,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Confirms VaultAllocated rows are written to `adapter_allocations`.
/// Test plan: `cargo test -p explorer-indexer -- adapter_allocation`
#[tokio::test]
async fn adapter_allocation_allocated_rows_written() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);
    let adapter_addr = Address::from([0xBBu8; 20]);

    let allocated_log = encode_allocated_log(
        vault_addr,
        adapter_addr,
        U256::from(0u64),
        U256::from(500_000u64),
        50u64,
        [0x11u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub.set("eth_getLogs", serde_json::Value::Array(vec![allocated_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::AdapterAllocations).await.unwrap();
    assert_eq!(
        count, 1,
        "adapter_allocations must have one row after Allocated event"
    );

    // Verify event_kind and amount.
    let row: (String, bigdecimal::BigDecimal, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT event_kind, amount, adapter FROM adapter_allocations WHERE chain_id = $1",
    )
    .bind(8453i64)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(row.0, "allocated");
    assert_eq!(row.2.as_deref(), Some(adapter_addr.as_slice()));
}

/// Confirms VaultPulled rows are written to `adapter_allocations` (AC: adapter_allocation).
#[tokio::test]
async fn adapter_allocation_pulled_rows_written() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);
    let adapter_addr = Address::from([0xBBu8; 20]);

    let pulled_log = encode_pulled_log(
        vault_addr,
        adapter_addr,
        U256::from(0u64),
        U256::from(200_000u64),
        50u64,
        [0x22u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub.set("eth_getLogs", serde_json::Value::Array(vec![pulled_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::AdapterAllocations).await.unwrap();
    assert_eq!(
        count, 1,
        "adapter_allocations must have one row after Pulled event"
    );

    let row: (String,) =
        sqlx::query_as("SELECT event_kind FROM adapter_allocations WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(row.0, "pulled");
}

/// Confirms VaultRebalanced rows are written to `adapter_allocations` (AC: adapter_allocation).
/// The adapter and adapter_index columns must be NULL for Rebalanced events.
#[tokio::test]
async fn adapter_allocation_rebalanced_rows_written() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);

    let rebalanced_log =
        encode_rebalanced_log(vault_addr, U256::from(300_000u64), 50u64, [0x33u8; 32], 0);

    let stub = StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub.set(
        "eth_getLogs",
        serde_json::Value::Array(vec![rebalanced_log]),
    );
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::AdapterAllocations).await.unwrap();
    assert_eq!(
        count, 1,
        "adapter_allocations must have one row after Rebalanced event"
    );

    // Adapter and adapter_index must be NULL for Rebalanced.
    let row: (String, Option<Vec<u8>>, Option<i64>) = sqlx::query_as(
        "SELECT event_kind, adapter, adapter_index FROM adapter_allocations WHERE chain_id = $1",
    )
    .bind(8453i64)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(row.0, "rebalanced");
    assert!(row.1.is_none(), "adapter must be NULL for Rebalanced event");
    assert!(
        row.2.is_none(),
        "adapter_index must be NULL for Rebalanced event"
    );
}

/// Confirms ExitFeeCharged rows are written to `vault_fee_events`.
/// Test plan: `cargo test -p explorer-indexer -- fee_charged`
#[tokio::test]
async fn fee_charged_rows_written() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);
    let owner = Address::from([0x11u8; 20]);
    let receiver = Address::from([0x22u8; 20]);

    let fee_log = encode_exit_fee_charged_log(
        vault_addr,
        owner,
        receiver,
        U256::from(1_000_000u64),
        U256::from(5_000u64),
        U256::from(995_000u64),
        50u64,
        [0x44u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub.set("eth_getLogs", serde_json::Value::Array(vec![fee_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::VaultFeeEvents).await.unwrap();
    assert_eq!(
        count, 1,
        "vault_fee_events must have one row after ExitFeeCharged event"
    );

    // Verify owner and receiver addresses.
    let row: (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT owner, receiver FROM vault_fee_events WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(row.0, owner.as_slice(), "owner address must match");
    assert_eq!(row.1, receiver.as_slice(), "receiver address must match");
}

/// Confirms ERC-4626 Deposit events are written to `vault_transfer_events`
/// with direction='deposit'.
#[tokio::test]
async fn erc4626_deposit_rows_written() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);
    let caller = Address::from([0x33u8; 20]);
    let owner = Address::from([0x44u8; 20]);

    let deposit_log = encode_erc4626_deposit_log(
        vault_addr,
        caller,
        owner,
        U256::from(1_000_000u64),
        U256::from(1_000_000u64),
        50u64,
        [0x55u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub.set("eth_getLogs", serde_json::Value::Array(vec![deposit_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    let count = fx.db.count(CountTable::VaultTransferEvents).await.unwrap();
    assert_eq!(
        count, 1,
        "vault_transfer_events must have one row after ERC-4626 Deposit event"
    );

    let row: (String,) =
        sqlx::query_as("SELECT direction FROM vault_transfer_events WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(row.0, "deposit");
}

/// Regression for issue #989: a single ERC-4626 Withdraw log must persist BOTH a
/// vault_transfer_events row with direction='withdrawal' (the table the watchdog
/// burn-volume circuit breaker reads) AND the owner's account_history_events
/// withdrawal row. Previously two `topic0 == erc4626_withdraw` branches existed;
/// the first returned early, making the vault_transfer_events writer dead code and
/// stranding the burn alarm. The branches are now merged into one writer.
#[tokio::test]
async fn erc4626_withdraw_writes_both_transfer_and_history_rows() {
    let fx = pg_fixture().await;

    let vault_addr = Address::from([0xAAu8; 20]);
    let caller = Address::from([0x33u8; 20]);
    let receiver = Address::from([0x44u8; 20]);
    let owner = Address::from([0x55u8; 20]);

    let withdraw_log = encode_erc4626_withdraw_log(
        vault_addr,
        caller,
        receiver,
        owner,
        U256::from(2_000_000u64),
        U256::from(2_000_000u64),
        50u64,
        [0x66u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub.set("eth_getLogs", serde_json::Value::Array(vec![withdraw_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));

    let rpc = JsonRpc::new(&stub.url);
    let cfg = base_config(vault_addr);

    let o = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(o.error.is_none(), "tick must succeed: {:?}", o.error);
    stub.shutdown();

    // Exactly one vault_transfer_events row with direction='withdrawal'.
    let transfer_count = fx.db.count(CountTable::VaultTransferEvents).await.unwrap();
    assert_eq!(
        transfer_count, 1,
        "exactly one vault_transfer_events row expected after ERC-4626 Withdraw"
    );
    let transfer_row: (String,) =
        sqlx::query_as("SELECT direction FROM vault_transfer_events WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(
        transfer_row.0, "withdrawal",
        "vault_transfer_events.direction must be 'withdrawal' (feeds watchdog burn alarm)"
    );

    // The owner's account_history_events withdrawal row is still written (no regression).
    let history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM account_history_events \
         WHERE chain_id = $1 AND kind = 'withdrawal'",
    )
    .bind(8453i64)
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(
        history_count, 1,
        "owner's account_history_events withdrawal row must still be written"
    );
}

/// Structural guard for issue #989: `handle_log` must contain exactly one branch
/// keyed on `topics.erc4626_withdraw`. A second branch is unreachable (the first
/// returns early), which is the defect that stranded the watchdog burn alarm.
/// This test fails if a duplicate branch is ever reintroduced. Runs without a DB.
#[test]
fn handle_log_has_single_erc4626_withdraw_branch() {
    let src = include_str!("../src/indexer.rs");
    let matches = src.matches("topic0 == topics.erc4626_withdraw").count();
    assert_eq!(
        matches, 1,
        "expected exactly one `topic0 == topics.erc4626_withdraw` branch in handle_log, \
         found {matches}; a duplicate branch is unreachable and strands the watchdog burn alarm"
    );
}
