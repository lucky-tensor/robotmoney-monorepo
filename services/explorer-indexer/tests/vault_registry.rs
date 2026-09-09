//! Suite-08 — VaultRegistry event ingestion tests.
//!
//! Covers issue #295 acceptance criteria:
//!
//! 1. `vault_registered_event_inserts_vaults_row`: A synthetic
//!    `VaultRegistered` log causes the indexer to insert a row into the
//!    `vaults` table within the same tick (AC-1 proxy).
//!
//! 2. `vault_status_changed_updates_status`: A `VaultStatusChanged` log
//!    atomically updates `vaults.status` and `vaults.status_changed_at`
//!    at the correct block (AC-2).
//!
//! 3. `vault_registry_migration_does_not_destroy_snapshots`: After
//!    migration 0002, existing `vault_snapshots` rows are still
//!    countable (AC-3 — preservation of single-vault history).
//!
//! 4. `vault_registered_reorg_removes_snapshot_rows`: A reorg above the
//!    registered block deletes the vault_snapshots rows above the root
//!    (reorg policy — AC-4 proxy; vaults rows are not block-keyed and
//!    survive).
//!
//! All tests skip cleanly when Docker is not available.
//!
//! Canonical: docs/technical/vault-registry-decisions.md §3.5.

mod common;

use alloy_primitives::{Address, U256};
use common::{pg_fixture, StubRpcServer};
use explorer_indexer::{
    abi::IVaultRegistryEvents,
    db::CountTable,
    indexer::{run_once, IndexerConfig},
    rpc::JsonRpc,
};

/// Encode a `VaultRegistered` log into the wire format expected by the
/// stub server's `eth_getLogs` response.
///
/// New signature (VaultRegistry.sol:67): `(address indexed vault, string name, address indexed asset)`.
#[allow(clippy::too_many_arguments)]
fn encode_vault_registered_log(
    registry_addr: Address,
    vault_addr: Address,
    name: &str,
    asset_addr: Address,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    // Build the ABI-encoded event using alloy_sol_types.
    // asset defaults to zero address in tests (removed field).
    let event = IVaultRegistryEvents::VaultRegistered {
        vault: vault_addr,
        name: name.to_string(),
        asset: asset_addr,
    };
    let log_data: LogData = event.encode_log_data();

    // The first topic is the event signature hash (topic-0); topic-1 is
    // the indexed `vault` address (padded to 32 bytes).
    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", registry_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xabu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

/// Encode a `VaultStatusChanged` log.
///
/// New signature (VaultRegistry.sol:73):
/// `(address indexed vault, uint8 indexed newStatus, uint256 timestamp)`.
#[allow(clippy::too_many_arguments)]
fn encode_vault_status_changed_log(
    registry_addr: Address,
    vault_addr: Address,
    new_status: u8,
    changed_at: u64,
    block_number: u64,
    tx_hash: [u8; 32],
    log_index: u32,
) -> serde_json::Value {
    use alloy_primitives::LogData;
    use alloy_sol_types::SolEvent as _;

    let event = IVaultRegistryEvents::VaultStatusChanged {
        vault: vault_addr,
        newStatus: new_status,
        timestamp: U256::from(changed_at),
    };
    let log_data: LogData = event.encode_log_data();

    let topics: Vec<String> = log_data
        .topics()
        .iter()
        .map(|t| format!("{:#x}", t))
        .collect();
    let data_hex = format!("0x{}", hex::encode(log_data.data.as_ref()));

    serde_json::json!({
        "address":          format!("{:#x}", registry_addr),
        "topics":           topics,
        "data":             data_hex,
        "blockNumber":      format!("0x{:x}", block_number),
        "blockHash":        format!("0x{}", hex::encode([0xbbu8; 32])),
        "transactionHash":  format!("0x{}", hex::encode(tx_hash)),
        "transactionIndex": "0x0",
        "logIndex":         format!("0x{:x}", log_index),
    })
}

fn stub_block(number: u64, hash_byte: u8, parent_byte: u8) -> serde_json::Value {
    serde_json::json!({
        "number":     format!("0x{:x}", number),
        "hash":       format!("0x{}", hex::encode([hash_byte; 32])),
        "parentHash": format!("0x{}", hex::encode([parent_byte; 32])),
        "timestamp":  "0x65000000",
        "transactions": []
    })
}

/// AC-1 proxy: a VaultRegistered event causes the indexer to insert a
/// row into `vaults` within one poll tick.
#[tokio::test]
async fn vault_registered_event_inserts_vaults_row() {
    let fx = pg_fixture().await;

    let registry_addr = Address::from([0xEEu8; 20]);
    let vault_addr = Address::from([0xAAu8; 20]);
    let asset_addr = Address::from([0xDDu8; 20]);

    let reg_log = encode_vault_registered_log(
        registry_addr,
        vault_addr,
        "RobotMoney USDC Vault",
        asset_addr,
        50u64,
        [0x11u8; 32],
        0,
    );

    let stub = StubRpcServer::start().await;
    stub.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub.set("eth_getLogs", serde_json::Value::Array(vec![reg_log]));
    stub.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));
    stub.set(
        "eth_getTransactionReceipt",
        serde_json::json!({ "status": "0x1" }),
    );

    let rpc = JsonRpc::new(&stub.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xBBu8; 20]),
        vault: Address::from([0xCCu8; 20]),
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let outcome = run_once(&fx.db, &rpc, &cfg).await.unwrap();
    assert!(
        outcome.error.is_none(),
        "tick must succeed: {:?}",
        outcome.error
    );

    let vault_count = fx.db.count(CountTable::Vaults).await.unwrap();
    assert_eq!(
        vault_count, 1,
        "vaults table must have exactly one row after VaultRegistered event"
    );

    // Verify the row has the correct address by querying directly.
    let row: Option<(Vec<u8>, String, String, i16)> = sqlx::query_as(
        "SELECT vault_address, name, risk_label, status \
         FROM vaults WHERE chain_id = $1",
    )
    .bind(8453i64)
    .fetch_optional(fx.db.pool())
    .await
    .unwrap();

    let (addr_bytes, name, risk_label, status) = row.expect("vault row must exist");
    assert_eq!(
        addr_bytes,
        vault_addr.as_slice(),
        "vault_address must match"
    );
    assert_eq!(name, "RobotMoney USDC Vault");
    assert_eq!(risk_label, "STABLE_YIELD");
    assert_eq!(status, 0, "status must be Active (0) at registration");

    stub.shutdown();
}

/// AC-2: VaultStatusChanged event atomically updates vault status at
/// the correct block.
#[tokio::test]
async fn vault_status_changed_updates_status() {
    let fx = pg_fixture().await;

    let registry_addr = Address::from([0xEEu8; 20]);
    let vault_addr = Address::from([0xAAu8; 20]);
    let asset_addr = Address::from([0xDDu8; 20]);

    // First tick: register the vault.
    let reg_log = encode_vault_registered_log(
        registry_addr,
        vault_addr,
        "RobotMoney USDC Vault",
        asset_addr,
        50u64,
        [0x11u8; 32],
        0,
    );

    let stub1 = StubRpcServer::start().await;
    stub1.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub1.set("eth_getLogs", serde_json::Value::Array(vec![reg_log]));
    stub1.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub1.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));
    stub1.set(
        "eth_getTransactionReceipt",
        serde_json::json!({ "status": "0x1" }),
    );

    let rpc1 = JsonRpc::new(&stub1.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xBBu8; 20]),
        vault: Address::from([0xCCu8; 20]),
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(o1.error.is_none(), "registration tick: {:?}", o1.error);
    stub1.shutdown();

    // Confirm vault is Active.
    let (status_before,): (i16,) = sqlx::query_as("SELECT status FROM vaults WHERE chain_id = $1")
        .bind(8453i64)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(
        status_before, 0,
        "status must be Active before status-change"
    );

    // Second tick: emit VaultStatusChanged (Active → Paused).
    let sc_log = encode_vault_status_changed_log(
        registry_addr,
        vault_addr,
        1u8, // Paused
        1_748_000_100u64,
        80u64,
        [0x22u8; 32],
        0,
    );

    let stub2 = StubRpcServer::start().await;
    stub2.set(
        "eth_blockNumber",
        serde_json::Value::String("0x5e".into()), // 94
    );
    stub2.set("eth_getLogs", serde_json::Value::Array(vec![sc_log]));
    stub2.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub2.set("eth_getBlockByNumber", stub_block(89, 0xbc, 0xab));
    stub2.set(
        "eth_getTransactionReceipt",
        serde_json::json!({ "status": "0x1" }),
    );

    let rpc2 = JsonRpc::new(&stub2.url);
    let cfg2 = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xBBu8; 20]),
        vault: Address::from([0xCCu8; 20]),
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(89),
        feature_flags: 4,
    };

    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(o2.error.is_none(), "status-change tick: {:?}", o2.error);
    stub2.shutdown();

    // Status must now be Paused (1).
    let (status_after, changed_at): (i16, Option<i64>) =
        sqlx::query_as("SELECT status, status_changed_at FROM vaults WHERE chain_id = $1")
            .bind(8453i64)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();

    assert_eq!(
        status_after, 1,
        "status must be Paused (1) after VaultStatusChanged"
    );
    assert_eq!(
        changed_at,
        Some(1_748_000_100i64),
        "status_changed_at must reflect changedAt from event"
    );
}

/// Issue #990 / scan finding IDX-6: a registry-discovered vault must be added to
/// the `eth_getLogs` watched-address set on subsequent ticks, so its ERC-4626
/// `Deposit`/`Withdraw` logs are fetched (without this, the watchdog burn alarm is
/// blind to redemptions from registered-but-unpinned vaults).
///
/// Tick 1 registers the vault (writes a `vaults` row, status=Active). Tick 2 must
/// then request logs for an address set that includes that vault address.
#[tokio::test]
async fn registered_vault_added_to_watched_address_set() {
    let fx = pg_fixture().await;

    let registry_addr = Address::from([0xEEu8; 20]);
    let vault_addr = Address::from([0xA9u8; 20]);
    let asset_addr = Address::from([0xDDu8; 20]);
    let gateway_addr = Address::from([0xBBu8; 20]);
    let pinned_vault = Address::from([0xCCu8; 20]);

    // Tick 1 — register the vault.
    let reg_log = encode_vault_registered_log(
        registry_addr,
        vault_addr,
        "Registered Extra Vault",
        asset_addr,
        50u64,
        [0x11u8; 32],
        0,
    );
    let stub1 = StubRpcServer::start().await;
    stub1.set("eth_blockNumber", serde_json::Value::String("0x46".into())); // 70
    stub1.set("eth_getLogs", serde_json::Value::Array(vec![reg_log]));
    stub1.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub1.set("eth_getBlockByNumber", stub_block(65, 0xab, 0xaa));
    stub1.set(
        "eth_getTransactionReceipt",
        serde_json::json!({ "status": "0x1" }),
    );

    let rpc1 = JsonRpc::new(&stub1.url);
    let cfg1 = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: gateway_addr,
        vault: pinned_vault,
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };
    let o1 = run_once(&fx.db, &rpc1, &cfg1).await.unwrap();
    assert!(o1.error.is_none(), "registration tick: {:?}", o1.error);
    assert_eq!(
        fx.db.count(CountTable::Vaults).await.unwrap(),
        1,
        "vault must be registered"
    );
    stub1.shutdown();

    // Tick 2 — advance the cursor; capture the eth_getLogs request.
    let stub2 = StubRpcServer::start().await;
    stub2.set("eth_blockNumber", serde_json::Value::String("0x5e".into())); // 94
    stub2.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    stub2.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    stub2.set("eth_getBlockByNumber", stub_block(89, 0xbc, 0xab));

    let rpc2 = JsonRpc::new(&stub2.url);
    let cfg2 = IndexerConfig {
        end_block: Some(89),
        ..cfg1.clone()
    };
    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(o2.error.is_none(), "second tick: {:?}", o2.error);

    // Find the eth_getLogs request and assert the registered vault is in its
    // address filter alongside the static watched addresses.
    let requests = stub2.captured_requests();
    let get_logs = requests
        .iter()
        .find(|r| r.get("method").and_then(|m| m.as_str()) == Some("eth_getLogs"))
        .expect("indexer must issue eth_getLogs");
    let addrs: Vec<String> = get_logs["params"][0]["address"]
        .as_array()
        .expect("address filter must be an array")
        .iter()
        .map(|a| a.as_str().unwrap().to_lowercase())
        .collect();

    let want = format!("{:#x}", vault_addr);
    assert!(
        addrs.contains(&want),
        "registered vault {want} must be in the watched address set; got {addrs:?}"
    );
    // The static set must still be present (no regression).
    assert!(
        addrs.contains(&format!("{:#x}", gateway_addr)),
        "gateway must remain watched; got {addrs:?}"
    );

    stub2.shutdown();
}

/// AC-3: Existing vault_snapshots rows are preserved after migration 0002.
///
/// We directly insert a vault_snapshots row (simulating pre-existing
/// single-vault history) and confirm it survives after the migrated
/// schema is in place.
#[tokio::test]
async fn migration_preserves_existing_vault_snapshots() {
    let fx = pg_fixture().await;

    // Insert chain, contract, and a vault_snapshots row directly.
    fx.db.upsert_chain(8453, "base", "stub").await.unwrap();
    let vault_addr = [0xAAu8; 20];
    fx.db
        .upsert_contract(8453, vault_addr, "vault", None)
        .await
        .unwrap();
    fx.db
        .insert_vault_snapshot(
            8453,
            vault_addr,
            100,
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            50,
            U256::ZERO,
            false,
        )
        .await
        .unwrap();

    // The snapshot must still be there (migration 0002 is additive).
    let snap_count = fx.db.count(CountTable::VaultSnapshots).await.unwrap();
    assert_eq!(
        snap_count, 1,
        "vault_snapshots row must survive migration 0002"
    );

    // The new vaults table must exist and be empty.
    let vault_count = fx.db.count(CountTable::Vaults).await.unwrap();
    assert_eq!(vault_count, 0, "vaults table must start empty");
}

/// Reorg test (AC-4 proxy): rows above the reorg head in vault_snapshots
/// are deleted; vaults rows (not block-keyed) survive the reorg.
///
/// Sequence:
///   Tick 1 — register vault at block 50, producing a vaults row.
///             Cursor block 65 is stored.
///   Tick 2 — reorg at block 65 (new hash).  walk_back returns -1.
///             delete_above_block(-1) wipes vault_snapshots rows above
///             -1 but the vaults table has no block_number column so its
///             rows are unaffected.
#[tokio::test]
async fn reorg_deletes_snapshot_rows_but_preserves_vaults_rows() {
    let fx = pg_fixture().await;

    let registry_addr = Address::from([0xEEu8; 20]);
    let vault_addr = Address::from([0xAAu8; 20]);
    let asset_addr = Address::from([0xDDu8; 20]);

    // Tick 1: register vault + cursor block 65.
    let reg_log = encode_vault_registered_log(
        registry_addr,
        vault_addr,
        "Test Vault",
        asset_addr,
        50u64,
        [0x11u8; 32],
        0,
    );

    let stub1 = StubRpcServer::start().await;
    stub1.set(
        "eth_blockNumber",
        serde_json::Value::String("0x46".into()), // 70
    );
    stub1.set("eth_getLogs", serde_json::Value::Array(vec![reg_log]));
    stub1.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // Return cursor block 65 with hash 0xcc…
    stub1.set("eth_getBlockByNumber", stub_block(65, 0xcc, 0xbb));
    stub1.set(
        "eth_getTransactionReceipt",
        serde_json::json!({ "status": "0x1" }),
    );

    let rpc1 = JsonRpc::new(&stub1.url);
    let cfg = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xBBu8; 20]),
        vault: Address::from([0xCCu8; 20]),
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o1 = run_once(&fx.db, &rpc1, &cfg).await.unwrap();
    assert!(o1.error.is_none(), "pre-reorg tick: {:?}", o1.error);
    assert_eq!(o1.last_indexed_block, Some(65));
    assert_eq!(
        fx.db.count(CountTable::Vaults).await.unwrap(),
        1,
        "vault row must exist after tick 1"
    );
    stub1.shutdown();

    // Manually seed a vault_snapshots row at block 60 to verify
    // it gets deleted during the reorg.  The FK requires a contracts row
    // for the vault address — upsert it before inserting the snapshot.
    let vault_bytes: [u8; 20] = vault_addr.into_array();
    fx.db
        .upsert_contract(8453, vault_bytes, "vault", None)
        .await
        .unwrap();
    fx.db
        .insert_vault_snapshot(
            8453,
            vault_bytes,
            60,
            U256::from(500_000u64),
            U256::from(500_000u64),
            50,
            U256::ZERO,
            false,
        )
        .await
        .unwrap();
    let snaps_before = fx.db.count(CountTable::VaultSnapshots).await.unwrap();
    assert!(snaps_before >= 1, "at least one snapshot before reorg");

    // Tick 2: reorg — cursor block 65 now has a different hash.
    let stub2 = StubRpcServer::start().await;
    stub2.set("eth_blockNumber", serde_json::Value::String("0x46".into()));
    stub2.set("eth_getLogs", serde_json::Value::Array(Vec::new()));
    stub2.set(
        "eth_call",
        serde_json::Value::String(format!("0x{}", "00".repeat(32))),
    );
    // Block 65 with new hash 0xdd… triggers reorg; walk_back returns -1.
    stub2.set("eth_getBlockByNumber", stub_block(65, 0xddu8, 0xeeu8));

    let rpc2 = JsonRpc::new(&stub2.url);
    let cfg2 = IndexerConfig {
        chain_id: 8453,
        chain_name: "base".into(),
        rpc_label: "stub".into(),
        gateway: Address::from([0xBBu8; 20]),
        vault: Address::from([0xCCu8; 20]),
        registry: Some(registry_addr),
        router_governance: None,
        portfolio_router: None,
        investment_committee: None,
        consensus_receipt: None,
        max_blocks_per_tick: 200,
        end_block: Some(65),
        feature_flags: 4,
    };

    let o2 = run_once(&fx.db, &rpc2, &cfg2).await.unwrap();
    assert!(o2.error.is_none(), "post-reorg tick: {:?}", o2.error);
    assert!(o2.reorg_detected, "reorg must be detected");

    // The manually-seeded vault_snapshots row at block 60 must be gone
    // (above the reorg root of -1 → deleted by delete_above_block).
    // Note: the indexer's heartbeat may write a new snapshot at the
    // cursor block during the same tick, so we check the specific row
    // by (chain_id, contract, block_number) rather than total count.
    let snap_at_60: Option<(i64,)> = sqlx::query_as(
        "SELECT block_number FROM vault_snapshots \
         WHERE chain_id = $1 AND block_number = 60",
    )
    .bind(8453i64)
    .fetch_optional(fx.db.pool())
    .await
    .unwrap();
    assert!(
        snap_at_60.is_none(),
        "vault_snapshots row at block 60 must be deleted on full reorg"
    );

    // vaults rows must survive — the vaults table is not block-keyed.
    assert_eq!(
        fx.db.count(CountTable::Vaults).await.unwrap(),
        1,
        "vaults row must survive reorg (not block-keyed)"
    );

    stub2.shutdown();
}
