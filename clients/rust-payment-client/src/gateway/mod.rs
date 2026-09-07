//! Canonical: Plan tracking issue #109 §3.2 — RobotMoneyGateway.sol (typed ABI bindings)
//!
//! `gateway` module — typed `alloy-sol-types` bindings for the on-chain
//! contracts the daemon interacts with.
//!
//! Per issue #11 and `Plan tracking issue #109` §3.5: typed ABI
//! encode/decode for `RobotMoneyGateway`, plus read-side bindings for the
//! standard ERC-20 `allowance`+`balanceOf` views (used against real USDC in
//! production and against test ERC-20 deployments in CI) and the `MockVault`
//! used by tests. The ABIs are extracted from the Foundry build output and
//! committed under `clients/rust-payment-client/abi/` so the Rust crate is
//! buildable without re-running `forge build`.
//!
//! Only the typed call/event/error structs are exposed — no provider
//! abstraction is built here. Tx construction lives in `tx`, and the
//! JSON-RPC transport lives in [`crate::rpc`]. Keeping those concerns
//! separate matches §3.5 ("the Rust binary must remain the only path to a
//! signed deposit tx; no alloy provider is exposed externally").

/// Expand one `sol!` binding inside its own private module, then re-export the
/// contract module so callers keep writing `crate::gateway::<Contract>`.
///
/// The wrapper is load-bearing, not cosmetic (issue #1362). `sol!` emits a
/// module per *namespace* it sees, and a full Foundry ABI names the declaring
/// contract of every struct it borrows: `PortfolioRouter.json` carries
/// `struct VaultRegistry.VaultMetadata`, so expanding it at file scope defines
/// a second `VaultRegistry` module alongside the real `VaultRegistry` binding
/// and the crate stops compiling. That collision is why these files were
/// hand-trimmed excerpts in the first place. One private module per binding
/// keeps every generated namespace local, so any binding can be the complete
/// artifact ABI without colliding with its neighbours.
macro_rules! sol_binding {
    ($module:ident, $contract:ident, $abi:literal) => {
        mod $module {
            alloy_sol_types::sol!(
                #[sol(abi)]
                #[allow(missing_docs, clippy::too_many_arguments)]
                $contract,
                $abi
            );
        }
        pub use $module::$contract;
    };
}

sol_binding!(
    robot_money_gateway,
    RobotMoneyGateway,
    "abi/RobotMoneyGateway.json"
);
sol_binding!(erc20, Erc20, "abi/Erc20.json");
sol_binding!(mock_vault, MockVault, "abi/MockVault.json");
sol_binding!(vault_registry, VaultRegistry, "abi/VaultRegistry.json");
sol_binding!(
    portfolio_router,
    PortfolioRouter,
    "abi/PortfolioRouter.json"
);
sol_binding!(
    router_governance,
    RouterGovernance,
    "abi/RouterGovernance.json"
);
sol_binding!(
    timelock_controller,
    TimelockController,
    "abi/TimelockController.json"
);
sol_binding!(
    investment_committee_policy,
    InvestmentCommitteePolicy,
    "abi/InvestmentCommitteePolicy.json"
);
sol_binding!(
    consensus_recommendation_receipt,
    ConsensusRecommendationReceipt,
    "abi/ConsensusRecommendationReceipt.json"
);

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, keccak256, Address, Bytes, LogData, B256, U256};
    use alloy_sol_types::{sol, SolCall, SolError, SolEvent};

    /// The `deposit` selector must match `keccak256("deposit(bytes32,uint256,uint64,bytes32)")[..4]`.
    /// This is the load-bearing cross-check that the generated bindings line
    /// up with the Solidity ABI committed in `contracts/gateway/`.
    #[test]
    fn deposit_selector_matches_canonical_signature() {
        let canonical = "deposit(bytes32,uint256,uint64,bytes32)";
        let expected = &keccak256(canonical.as_bytes())[..4];
        let actual = RobotMoneyGateway::depositCall::SELECTOR;
        assert_eq!(&actual, expected, "deposit selector drift");
    }

    /// The `depositTo` selector must match
    /// `keccak256("depositTo(bytes32,uint256,uint64,bytes32,address,uint256[])")[..4]`.
    /// This cross-checks the router-deposit ABI binding added in issue #649.
    #[test]
    fn deposit_to_selector_matches_canonical_signature() {
        let canonical = "depositTo(bytes32,uint256,uint64,bytes32,address,uint256[])";
        let expected = &keccak256(canonical.as_bytes())[..4];
        let actual = RobotMoneyGateway::depositToCall::SELECTOR;
        assert_eq!(&actual, expected, "depositTo selector drift");
    }

    /// `authorizeAgent(address,(bool,uint64,uint256,uint256,address,address[],address,uint256,uint256,address[]))` —
    /// ensure the tuple layout matches the on-chain ABI (withdrawal fields added in #311).
    #[test]
    fn authorize_agent_selector_matches() {
        let canonical = "authorizeAgent(address,(bool,uint64,uint256,uint256,address,address[],address,uint256,uint256,address[]))";
        let expected = &keccak256(canonical.as_bytes())[..4];
        let actual = RobotMoneyGateway::authorizeAgentCall::SELECTOR;
        assert_eq!(&actual, expected);
    }

    #[test]
    fn paused_view_selector_matches() {
        let expected = &keccak256(b"paused()")[..4];
        let actual = RobotMoneyGateway::pausedCall::SELECTOR;
        assert_eq!(&actual, expected);
    }

    #[test]
    fn agent_not_authorized_error_selector_matches() {
        let expected = &keccak256(b"AgentNotAuthorized()")[..4];
        let actual = RobotMoneyGateway::AgentNotAuthorized::SELECTOR;
        assert_eq!(&actual, expected);
    }

    /// Round-trip an `AgentDeposit` log: ABI-encode a synthetic event, then
    /// decode it back through the bindings. This exercises both the topic0
    /// hash and the data layout (3 indexed + 4 unindexed fields).
    #[test]
    fn agent_deposit_event_roundtrip() {
        let payment_id = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let order_id = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let agent: Address = address!("00000000000000000000000000000000000000aa");
        let share_receiver: Address = address!("00000000000000000000000000000000000000bb");
        let amount = U256::from(123_456u64);
        let shares = U256::from(987_654u64);
        let window_id = 42u64;

        let ev = RobotMoneyGateway::AgentDeposit {
            paymentId: payment_id,
            orderId: order_id,
            agent,
            shareReceiver: share_receiver,
            amount,
            sharesMinted: shares,
            windowId: window_id,
        };

        let topics = ev.encode_topics();
        let data: Vec<u8> = ev.encode_data();
        let log = LogData::new_unchecked(
            topics.iter().map(|t| B256::from(t.0)).collect(),
            Bytes::from(data),
        );

        let decoded =
            RobotMoneyGateway::AgentDeposit::decode_log_data(&log, true).expect("decode log");

        assert_eq!(decoded.paymentId, payment_id);
        assert_eq!(decoded.orderId, order_id);
        assert_eq!(decoded.agent, agent);
        assert_eq!(decoded.shareReceiver, share_receiver);
        assert_eq!(decoded.amount, amount);
        assert_eq!(decoded.sharesMinted, shares);
        assert_eq!(decoded.windowId, window_id);

        let expected_topic0 =
            keccak256(b"AgentDeposit(bytes32,bytes32,address,address,uint256,uint256,uint64)");
        assert_eq!(B256::from(topics[0].0), expected_topic0);
    }

    /// Encoding `agents(address)` and decoding the 8-tuple return value
    /// proves the view bindings line up. We hand-roll the return blob from
    /// `(bool,uint64,uint256,uint256,address,address,uint256,uint256)` so
    /// the test does not depend on a live RPC. Fields added in issue #311:
    /// assetRecipient, maxWithdrawPerPayment, maxWithdrawPerWindow.
    #[test]
    fn agents_view_decodes_8_tuple() {
        let mut blob = Vec::with_capacity(32 * 8);
        // active = true
        let mut w = [0u8; 32];
        w[31] = 1;
        blob.extend_from_slice(&w);
        // validUntil = 1_700_000_000
        let v: u64 = 1_700_000_000;
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&v.to_be_bytes());
        blob.extend_from_slice(&w);
        // maxPerPayment = 1_000_000 (1 USDC, 6 decimals)
        blob.extend_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>());
        // maxPerWindow = 100_000_000 (100 USDC)
        blob.extend_from_slice(&U256::from(100_000_000u64).to_be_bytes::<32>());
        // shareReceiver
        let recv: Address = address!("00000000000000000000000000000000000000cd");
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(recv.as_slice());
        blob.extend_from_slice(&w);
        // assetRecipient (added in #311)
        let asset_recv: Address = address!("00000000000000000000000000000000000000ef");
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(asset_recv.as_slice());
        blob.extend_from_slice(&w);
        // maxWithdrawPerPayment = 500_000 (added in #311)
        blob.extend_from_slice(&U256::from(500_000u64).to_be_bytes::<32>());
        // maxWithdrawPerWindow = 5_000_000 (added in #311)
        blob.extend_from_slice(&U256::from(5_000_000u64).to_be_bytes::<32>());

        let decoded =
            RobotMoneyGateway::agentsCall::abi_decode_returns(&blob, true).expect("decode");
        assert!(decoded.active);
        assert_eq!(decoded.validUntil, v);
        assert_eq!(decoded.maxPerPayment, U256::from(1_000_000u64));
        assert_eq!(decoded.maxPerWindow, U256::from(100_000_000u64));
        assert_eq!(decoded.shareReceiver, recv);
        assert_eq!(decoded.assetRecipient, asset_recv);
        assert_eq!(decoded.maxWithdrawPerPayment, U256::from(500_000u64));
        assert_eq!(decoded.maxWithdrawPerWindow, U256::from(5_000_000u64));
    }

    // The nine-field `VaultRecord` that `abi/VaultRegistry.json` declared for
    // `getVault` before issue #1362 — an aspirational shape from
    // `docs/technical/vault-registry-decisions.md` §3.4 that the shipped
    // `VaultRegistry.sol` (#329) never implemented. Kept here, and only here,
    // as the negative control for the test below.
    sol! {
        #[allow(missing_docs)]
        struct StaleVaultRecord {
            address vault;
            string name;
            string riskLabel;
            string mandate;
            uint8 status;
            address receiptToken;
            uint256 depositCap;
            uint16 exitFeeBps;
            uint64 registeredAt;
        }

        #[allow(missing_docs)]
        function getVaultStale(address vault) external view returns (StaleVaultRecord);
    }

    /// `VaultRegistry.getVault` return data, laid out by hand from
    /// `contracts/VaultRegistry.sol` rather than from any binding:
    ///
    /// ```solidity
    /// struct VaultMetadata { string name; address asset; uint256 registeredAt; }
    /// function getVault(address) external view
    ///     returns (VaultMetadata memory metadata, VaultStatus status);
    /// ```
    ///
    /// Two top-level outputs, so the head is `[offset(metadata), status]` and
    /// the metadata tuple follows at that offset. Building the words by hand is
    /// the point: an encoder driven by `abi/VaultRegistry.json` would just
    /// round-trip whatever that file happens to claim.
    fn real_get_vault_return_data(
        name: &str,
        asset: Address,
        registered_at: u64,
        status: u8,
    ) -> Vec<u8> {
        fn word(n: u64) -> [u8; 32] {
            U256::from(n).to_be_bytes::<32>()
        }
        let mut blob = Vec::new();
        // head[0]: offset of `metadata`, past the two head words.
        blob.extend_from_slice(&word(0x40));
        // head[1]: `status` (uint8, right-aligned).
        blob.extend_from_slice(&word(status as u64));
        // metadata tuple, base = 0x40. Its own head is [offset(name), asset,
        // registeredAt]; `name` data starts one word past that head.
        blob.extend_from_slice(&word(0x60));
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(asset.as_slice());
        blob.extend_from_slice(&w);
        blob.extend_from_slice(&word(registered_at));
        blob.extend_from_slice(&word(name.len() as u64));
        let mut padded = name.as_bytes().to_vec();
        padded.resize(name.len().div_ceil(32) * 32, 0);
        blob.extend_from_slice(&padded);
        blob
    }

    /// `rmpc get-vaults` / `get-vault` must decode what the deployed
    /// `VaultRegistry` actually returns.
    ///
    /// Before issue #1362 the committed `abi/VaultRegistry.json` declared
    /// `getVault` as returning a single nine-field `VaultRecord`, so both
    /// commands' `abi_decode_returns` calls could not read a real registry
    /// response at all. The `assert!(... .is_err())` half is the negative
    /// control: it pins that this test discriminates between the two shapes
    /// rather than merely re-stating the current one.
    #[test]
    fn get_vault_decodes_the_registry_two_output_shape() {
        let asset: Address = address!("00000000000000000000000000000000000000aa");
        let blob = real_get_vault_return_data("Robot Money USDC", asset, 1_715_000_000, 2);

        let decoded = VaultRegistry::getVaultCall::abi_decode_returns(&blob, true)
            .expect("getVault must decode VaultRegistry.sol's (VaultMetadata, VaultStatus)");
        assert_eq!(decoded.metadata.name, "Robot Money USDC");
        assert_eq!(decoded.metadata.asset, asset);
        assert_eq!(decoded.metadata.registeredAt, U256::from(1_715_000_000u64));
        assert_eq!(decoded.status, 2, "VaultStatus::Retired");

        assert!(
            getVaultStaleCall::abi_decode_returns(&blob, true).is_err(),
            "the pre-#1362 nine-field VaultRecord must NOT decode real registry \
             return data — if it does, this test no longer proves anything"
        );
    }

    /// The ERC-20 read-only views we need for preflight: `allowance` and
    /// `balanceOf` selectors must match the canonical ones.
    #[test]
    fn erc20_view_selectors_match() {
        assert_eq!(
            &Erc20::allowanceCall::SELECTOR,
            &keccak256(b"allowance(address,address)")[..4]
        );
        assert_eq!(
            &Erc20::balanceOfCall::SELECTOR,
            &keccak256(b"balanceOf(address)")[..4]
        );
    }
}
