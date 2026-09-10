//! §11 acceptance criterion: re-indexing the same range must produce
//! zero net inserts. We exercise the `Db` upsert helpers directly with
//! a synthetic event row, run the same insert twice, and assert the
//! second call returns 0 rows_affected (ON CONFLICT DO NOTHING).

mod common;

use alloy_primitives::U256;
use common::pg_fixture;
use explorer_indexer::db::CountTable;

#[tokio::test]
async fn agent_deposit_insert_is_idempotent() {
    let fx = pg_fixture().await;
    fx.db.upsert_chain(8453, "base", "test").await.unwrap();
    let gateway = [0xaau8; 20];
    fx.db
        .upsert_contract(8453, gateway, "gateway", None)
        .await
        .unwrap();
    fx.db
        .insert_block(8453, 100, [0x11u8; 32], [0x10u8; 32], 1_700_000_000)
        .await
        .unwrap();

    let first = fx
        .db
        .insert_agent_deposit(
            8453,
            100,
            7,
            [0x22u8; 32],
            [0x33u8; 32],
            [0x44u8; 32],
            [0x55u8; 20],
            [0x66u8; 20],
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            42,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first, 1, "first insert lands");

    let second = fx
        .db
        .insert_agent_deposit(
            8453,
            100,
            7,
            [0x22u8; 32],
            [0x33u8; 32],
            [0x44u8; 32],
            [0x55u8; 20],
            [0x66u8; 20],
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            42,
            None,
        )
        .await
        .unwrap();
    assert_eq!(second, 0, "second insert is a no-op");

    assert_eq!(fx.db.count(CountTable::AgentDeposits).await.unwrap(), 1);
}

#[tokio::test]
async fn vault_snapshot_insert_is_idempotent() {
    let fx = pg_fixture().await;
    fx.db.upsert_chain(8453, "base", "test").await.unwrap();
    let vault = [0xbbu8; 20];
    fx.db
        .upsert_contract(8453, vault, "vault", None)
        .await
        .unwrap();
    let first = fx
        .db
        .insert_vault_snapshot(
            8453,
            vault,
            999,
            U256::from(1_234_567u64),
            U256::from(1_000_000u64),
            50,
            U256::from(10u64).pow(U256::from(18u64)),
            false,
        )
        .await
        .unwrap();
    assert_eq!(first, 1);
    let second = fx
        .db
        .insert_vault_snapshot(
            8453,
            vault,
            999,
            U256::from(1_234_567u64),
            U256::from(1_000_000u64),
            50,
            U256::from(10u64).pow(U256::from(18u64)),
            false,
        )
        .await
        .unwrap();
    assert_eq!(second, 0);
}

#[tokio::test]
async fn agent_policy_insert_roundtrip_with_owner() {
    let fx = pg_fixture().await;
    fx.db.upsert_chain(8453, "base", "test").await.unwrap();
    let gateway = [0xaau8; 20];
    fx.db
        .upsert_contract(8453, gateway, "gateway", None)
        .await
        .unwrap();
    fx.db
        .insert_block(8453, 200, [0x21u8; 32], [0x20u8; 32], 1_700_000_100)
        .await
        .unwrap();

    let agent = [0x77u8; 20];
    let owner = [0x88u8; 20];
    let tx_hash = [0x99u8; 32];

    // First insert: authorization with owner and window_usage_to_date.
    let first = fx
        .db
        .insert_agent_policy(
            8453,
            200,
            0,
            tx_hash,
            agent,
            Some(owner),
            false,
            Some(9_999_999_i64),
            Some(U256::from(500_000_u64)),
            Some(U256::from(1_000_000_u64)),
            Some(U256::from(123_456_u64)),
            Some(owner),
        )
        .await
        .unwrap();
    assert_eq!(first, 1, "first insert lands");

    // Second insert with same PK (chain_id, block_number, log_index) is a no-op.
    let second = fx
        .db
        .insert_agent_policy(
            8453,
            200,
            0,
            tx_hash,
            agent,
            Some(owner),
            false,
            Some(9_999_999_i64),
            Some(U256::from(500_000_u64)),
            Some(U256::from(1_000_000_u64)),
            Some(U256::from(123_456_u64)),
            Some(owner),
        )
        .await
        .unwrap();
    assert_eq!(
        second, 0,
        "second insert is a no-op (ON CONFLICT DO NOTHING)"
    );

    assert_eq!(fx.db.count(CountTable::AgentPolicies).await.unwrap(), 1);
}
