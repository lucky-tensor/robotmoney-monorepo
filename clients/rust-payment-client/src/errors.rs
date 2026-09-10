//! Canonical: Plan tracking issue #109 §4 — Phase 1 Rust client (operator-visible error catalog)
//!
//! Named error variants used across the rmpc codebase.
//!
//! Variant names are part of the operator-visible contract: the CLI prints
//! the variant name (via `Display`) on the failure path, and downstream
//! tooling matches on those strings. Renaming a variant is a breaking change.

use thiserror::Error;

/// Top-level error type for the payment daemon.
///
/// The `Err`-prefixed variant names are mandated by issue #7 / the MVP doc
/// so they match the on-the-wire/log strings. The clippy lint that flags
/// shared prefixes is therefore suppressed at the type level.
#[derive(Debug, Error)]
#[allow(clippy::enum_variant_names)]
pub enum RmpcError {
    #[error("ErrAgentNotAuthorized: agent address is not registered with the gateway")]
    ErrAgentNotAuthorized,

    #[error("ErrFeeCapExceeded: computed maxFeePerGas exceeds operator-configured cap")]
    ErrFeeCapExceeded,

    #[error("ErrConcurrentInvocation: another rmpc invocation already holds the agent lock")]
    ErrConcurrentInvocation,

    #[error("ErrCodeHashMismatch: keccak256(eth_getCode(gateway)) does not match pinned hash")]
    ErrCodeHashMismatch,

    #[error("ErrChainIdMismatch: RPC eth_chainId does not match configured chain_id")]
    ErrChainIdMismatch,

    #[error("ErrGatewayPaused: gateway contract reports paused() == true")]
    ErrGatewayPaused,

    #[error("ErrAllowanceInsufficient: USDC allowance(self, gateway) < amount")]
    ErrAllowanceInsufficient,

    #[error("ErrBalanceInsufficient: USDC balanceOf(self) < amount")]
    ErrBalanceInsufficient,

    #[error("ErrSoftwareSignerDisallowed: [signer].allow_software_fallback must be true")]
    ErrSoftwareSignerDisallowed,

    #[error(
        "ErrProductionSignerRequired: Base mainnet write commands require an HSM/KMS/device-bound signer; software keystores are non-production only"
    )]
    ErrProductionSignerRequired,

    /// The same `(order_id, idempotency_key, deadline)` tuple was
    /// already submitted from this client; the local replay cache
    /// returns the prior `tx_hash` instead of re-broadcasting. Audit
    /// finding M3.
    #[error(
        "ErrOrderIdAlreadySubmitted: order_id was already submitted (prior tx_hash={tx_hash})"
    )]
    ErrOrderIdAlreadySubmitted { tx_hash: String },

    /// The broadcast transaction was mined but reverted (`status == 0` in
    /// the receipt). Carries the transaction hash so operators can pull
    /// the trace.
    #[error("ErrTxReverted: transaction reverted on-chain (tx_hash={tx_hash})")]
    ErrTxReverted { tx_hash: String },

    /// The deposit landed in a block but the gateway emitted no
    /// `AgentDeposit` log — invariant violation. Operator must inspect.
    #[error("ErrAgentDepositLogMissing: receipt has no AgentDeposit log (tx_hash={tx_hash})")]
    ErrAgentDepositLogMissing { tx_hash: String },

    /// The vault being redeemed from is paused — hard refusal before signing.
    #[error("ErrVaultPaused: source vault reports paused() == true")]
    ErrVaultPaused,

    /// Shares to withdraw exceed the agent's `maxWithdrawPerPayment` policy cap.
    #[error("ErrWithdrawCapExceeded: shares exceed agent maxWithdrawPerPayment policy cap")]
    ErrWithdrawCapExceeded,

    /// The agent holds fewer vault shares than the requested withdrawal amount.
    #[error("ErrShareBalanceInsufficient: agent vault share balance < requested shares")]
    ErrShareBalanceInsufficient,

    /// The vault share allowance(agent, gateway) is less than the requested shares.
    #[error(
        "ErrShareAllowanceInsufficient: vault share allowance(agent, gateway) < requested shares"
    )]
    ErrShareAllowanceInsufficient,

    // ── Architecture §7.2 product reason codes ─────────────────────────────
    /// The target vault is disabled (not registered or de-listed).
    /// Maps to the `vault_disabled` product reason code.
    #[error("ErrVaultDisabled: target vault is not registered or has been disabled")]
    ErrVaultDisabled,

    /// The agent policy `validUntil` timestamp is in the past.
    /// Maps to the `expired_policy` product reason code.
    #[error("ErrPolicyExpired: agent policy has expired (validUntil < block.timestamp)")]
    ErrPolicyExpired,

    /// A required router leg is unavailable (vault paused, full, or de-listed).
    /// Maps to the `unavailable_leg` product reason code.
    #[error("ErrLegUnavailable: router leg vault is unavailable (paused, full, or de-listed)")]
    ErrLegUnavailable,

    /// The transaction would not satisfy the caller's `minSharesPerLeg` slippage bound.
    /// Maps to the `slippage_bound_exceeded` product reason code.
    #[error(
        "ErrSlippageBoundExceeded: estimated shares per leg fall below the caller's minimum bound"
    )]
    ErrSlippageBoundExceeded,

    /// The withdraw landed in a block but the gateway emitted no
    /// `AgentWithdrawal` log — invariant violation. Operator must inspect.
    #[error("ErrAgentWithdrawLogMissing: receipt has no AgentWithdrawal log (tx_hash={tx_hash})")]
    ErrAgentWithdrawLogMissing { tx_hash: String },

    /// Caller has already voted on this proposal with a different choice.
    /// On-chain the contract only records a single FOR vote per address;
    /// attempting to re-cast with a different direction is refused.
    #[error(
        "ErrVoteAlreadyCast: a different vote direction was already cast for proposal_id={proposal_id}"
    )]
    ErrVoteAlreadyCast { proposal_id: String },

    // ── Committee errors ───────────────────────────────────────────────────
    /// The caller's address is not on the IC policy allowlist.
    /// On-chain: `AgentNotAllowlisted` custom error on `submitVote`.
    /// Emitted when `eth_sendRawTransaction` is rejected with a revert that
    /// indicates the caller is not an allowlisted committee agent.
    #[error("ErrNotAllowlisted: caller address is not an allowlisted committee agent")]
    ErrNotAllowlisted,

    /// The operator config is missing the `ic_policy_address` field.
    /// Both `committee register` and `committee vote-submit` fail-closed
    /// before any on-chain write when this field is absent.
    #[error(
        "ErrIcContractNotConfigured: ic_policy_address is not set in the operator config; \
         add `ic_policy_address = \"0x...\"` to the TOML"
    )]
    ErrIcContractNotConfigured,

    #[error("ErrConfig: configuration error: {0}")]
    ErrConfig(String),

    #[error("ErrIo: I/O error: {0}")]
    ErrIo(#[from] std::io::Error),

    #[error("ErrTomlParse: TOML parse error: {0}")]
    ErrTomlParse(#[from] toml::de::Error),

    /// Transport-level RPC failure — DNS, TCP, TLS, HTTP non-2xx, etc.
    /// Anything that prevents us from getting a JSON-RPC response body.
    #[error("ErrRpcTransport: JSON-RPC transport error: {0}")]
    ErrRpcTransport(String),

    /// Server returned a JSON-RPC error object (`{ "error": { code, message } }`).
    /// Code is preserved verbatim — operator tooling matches on it.
    #[error("ErrRpcServer: JSON-RPC server error code={code} message={message}")]
    ErrRpcServer { code: i64, message: String },

    /// The response body was malformed: not JSON, missing `result`, or the
    /// `result` field could not be deserialised into the expected shape.
    #[error("ErrRpcDecode: JSON-RPC response decode error: {0}")]
    ErrRpcDecode(String),
}

impl RmpcError {
    /// The stable, operator-visible variant name for this error.
    ///
    /// This is the single owner of the variant-name table. Every command
    /// that prints an `error` field in its JSON refusal body calls this;
    /// no command module carries its own transcription (issue #1285).
    ///
    /// The match is deliberately **exhaustive with no wildcard arm**:
    /// adding a variant to [`RmpcError`] without extending this table is
    /// a compile error, which is what stops a new variant reaching
    /// operators as the string `ErrUnknown`. `docs/technical/
    /// rmpc-read-output-contract.md` §3.7 makes these names a contract
    /// downstream tooling matches on, so a silent fallback is a
    /// contract violation, not a stylistic wart.
    pub fn name(&self) -> &'static str {
        match self {
            RmpcError::ErrAgentNotAuthorized => "ErrAgentNotAuthorized",
            RmpcError::ErrFeeCapExceeded => "ErrFeeCapExceeded",
            RmpcError::ErrConcurrentInvocation => "ErrConcurrentInvocation",
            RmpcError::ErrCodeHashMismatch => "ErrCodeHashMismatch",
            RmpcError::ErrChainIdMismatch => "ErrChainIdMismatch",
            RmpcError::ErrGatewayPaused => "ErrGatewayPaused",
            RmpcError::ErrAllowanceInsufficient => "ErrAllowanceInsufficient",
            RmpcError::ErrBalanceInsufficient => "ErrBalanceInsufficient",
            RmpcError::ErrSoftwareSignerDisallowed => "ErrSoftwareSignerDisallowed",
            RmpcError::ErrProductionSignerRequired => "ErrProductionSignerRequired",
            RmpcError::ErrOrderIdAlreadySubmitted { .. } => "ErrOrderIdAlreadySubmitted",
            RmpcError::ErrTxReverted { .. } => "ErrTxReverted",
            RmpcError::ErrAgentDepositLogMissing { .. } => "ErrAgentDepositLogMissing",
            RmpcError::ErrVaultPaused => "ErrVaultPaused",
            RmpcError::ErrWithdrawCapExceeded => "ErrWithdrawCapExceeded",
            RmpcError::ErrShareBalanceInsufficient => "ErrShareBalanceInsufficient",
            RmpcError::ErrShareAllowanceInsufficient => "ErrShareAllowanceInsufficient",
            RmpcError::ErrVaultDisabled => "ErrVaultDisabled",
            RmpcError::ErrPolicyExpired => "ErrPolicyExpired",
            RmpcError::ErrLegUnavailable => "ErrLegUnavailable",
            RmpcError::ErrSlippageBoundExceeded => "ErrSlippageBoundExceeded",
            RmpcError::ErrAgentWithdrawLogMissing { .. } => "ErrAgentWithdrawLogMissing",
            RmpcError::ErrVoteAlreadyCast { .. } => "ErrVoteAlreadyCast",
            RmpcError::ErrNotAllowlisted => "ErrNotAllowlisted",
            RmpcError::ErrIcContractNotConfigured => "ErrIcContractNotConfigured",
            RmpcError::ErrConfig(_) => "ErrConfig",
            RmpcError::ErrIo(_) => "ErrIo",
            RmpcError::ErrTomlParse(_) => "ErrTomlParse",
            RmpcError::ErrRpcTransport(_) => "ErrRpcTransport",
            RmpcError::ErrRpcServer { .. } => "ErrRpcServer",
            RmpcError::ErrRpcDecode(_) => "ErrRpcDecode",
        }
    }
}

pub type Result<T> = std::result::Result<T, RmpcError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// One live sample per [`RmpcError`] variant.
    ///
    /// `every_declared_variant_is_sampled` below scrapes the enum body in
    /// this very file and fails if this list misses a variant, so adding a
    /// variant without extending this list is a red test — not a silent
    /// coverage hole.
    fn sample_variants() -> Vec<RmpcError> {
        vec![
            RmpcError::ErrAgentNotAuthorized,
            RmpcError::ErrFeeCapExceeded,
            RmpcError::ErrConcurrentInvocation,
            RmpcError::ErrCodeHashMismatch,
            RmpcError::ErrChainIdMismatch,
            RmpcError::ErrGatewayPaused,
            RmpcError::ErrAllowanceInsufficient,
            RmpcError::ErrBalanceInsufficient,
            RmpcError::ErrSoftwareSignerDisallowed,
            RmpcError::ErrProductionSignerRequired,
            RmpcError::ErrOrderIdAlreadySubmitted {
                tx_hash: "0x00".into(),
            },
            RmpcError::ErrTxReverted {
                tx_hash: "0x00".into(),
            },
            RmpcError::ErrAgentDepositLogMissing {
                tx_hash: "0x00".into(),
            },
            RmpcError::ErrVaultPaused,
            RmpcError::ErrWithdrawCapExceeded,
            RmpcError::ErrShareBalanceInsufficient,
            RmpcError::ErrShareAllowanceInsufficient,
            RmpcError::ErrVaultDisabled,
            RmpcError::ErrPolicyExpired,
            RmpcError::ErrLegUnavailable,
            RmpcError::ErrSlippageBoundExceeded,
            RmpcError::ErrAgentWithdrawLogMissing {
                tx_hash: "0x00".into(),
            },
            RmpcError::ErrVoteAlreadyCast {
                proposal_id: "1".into(),
            },
            RmpcError::ErrNotAllowlisted,
            RmpcError::ErrIcContractNotConfigured,
            RmpcError::ErrConfig("bad field".into()),
            RmpcError::ErrIo(std::io::Error::other("io")),
            RmpcError::ErrTomlParse(toml::from_str::<toml::Value>("=").unwrap_err()),
            RmpcError::ErrRpcTransport("transport".into()),
            RmpcError::ErrRpcServer {
                code: -32000,
                message: "server".into(),
            },
            RmpcError::ErrRpcDecode("decode".into()),
        ]
    }

    /// Variant identifiers as declared in the `enum RmpcError` body of this
    /// file. Enum variants sit at exactly four spaces of indentation;
    /// `RmpcError::…` paths inside `name()` and inside this test module are
    /// indented further, so they are not picked up.
    fn declared_variant_names() -> BTreeSet<String> {
        include_str!("errors.rs")
            .lines()
            .filter(|l| l.starts_with("    Err"))
            .map(|l| {
                l.trim_start()
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn every_declared_variant_is_sampled() {
        let declared = declared_variant_names();
        assert!(
            declared.len() > 25,
            "the enum-body scraper found only {} variants — it has drifted from the source layout",
            declared.len()
        );
        let sampled: BTreeSet<String> = sample_variants()
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(
            declared, sampled,
            "sample_variants() must carry exactly one sample per declared RmpcError variant; \
             extend it (and RmpcError::name()) when adding a variant",
        );
    }

    #[test]
    fn every_variant_has_a_distinct_name_matching_display() {
        let mut seen: BTreeSet<&'static str> = BTreeSet::new();
        for err in sample_variants() {
            let name = err.name();
            assert!(
                seen.insert(name),
                "two RmpcError variants share the operator-visible name {name:?}",
            );
            assert_ne!(
                name, "ErrUnknown",
                "no variant may render as the ErrUnknown fallback",
            );
            let rendered = format!("{err}");
            assert!(
                rendered.starts_with(name),
                "Display output {rendered:?} does not start with variant name {name:?}",
            );
        }
    }

    #[test]
    fn config_error_carries_message() {
        let e = RmpcError::ErrConfig("bad field".into());
        assert!(format!("{e}").contains("bad field"));
    }
}
