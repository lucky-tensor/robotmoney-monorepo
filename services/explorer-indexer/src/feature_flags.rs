//! Canonical: docs/architecture.md §5.4 — Explorer Indexer and API
//! (See also: config/feature-flags.json)
//! Cross-system feature flag registry — Rust surface.
//! Canonical source: config/feature-flags.json
//! Implements: issue #389
//!
//! Flag IDs are stable bit positions in the uint256 bitmap used on-chain
//! (`contracts/FeatureFlags.sol`) and in the TypeScript dapp
//! (`clients/dapp/src/feature-flags.ts`).
//! Never renumber an ID once assigned.
//!
//! Runtime control: set the `FEATURE_FLAGS` environment variable to a decimal
//! integer whose bits correspond to flag IDs.  Example: `FEATURE_FLAGS=5`
//! enables flags 0 (MULTI_VAULT_ENABLED) and 2 (INDEXER_MULTI_VAULT_EVENTS).

// ---------------------------------------------------------------------------
// Flag ID constants — mirror config/feature-flags.json exactly.
// ---------------------------------------------------------------------------

/// Bit 0 — Gates multi-vault UI surfaces in the dapp and multi-vault event
/// indexing paths in the explorer-indexer.
/// config/feature-flags.json id=0
pub const MULTI_VAULT_ENABLED: u8 = 0;

/// Bit 1 — Gates the PortfolioRouter deposit path in the dapp and the
/// RouterGovernance panel.
/// config/feature-flags.json id=1
pub const PORTFOLIO_ROUTER_ENABLED: u8 = 1;

/// Bit 2 — Gates VaultRegistered / VaultStatusChanged event ingestion in
/// the explorer-indexer.
/// config/feature-flags.json id=2
pub const INDEXER_MULTI_VAULT_EVENTS: u8 = 2;

// ---------------------------------------------------------------------------
// Bitmap helpers
// ---------------------------------------------------------------------------

/// Returns `true` iff `flag_id` is set in `bitmap`.
/// Mirrors `FeatureFlags.isEnabled(flagId, bitmap)` in Solidity.
#[inline]
pub fn is_enabled(flag_id: u8, bitmap: u64) -> bool {
    (bitmap >> flag_id) & 1 == 1
}

/// Parse the `FEATURE_FLAGS` environment variable (decimal integer) into a
/// bitmap.  Returns `0` (all flags off) when the variable is absent or
/// cannot be parsed.
pub fn bitmap_from_env() -> u64 {
    std::env::var("FEATURE_FLAGS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the three tests that mutate `FEATURE_FLAGS`.
    ///
    /// Issue #1434 (incidental). `set_var`/`remove_var` act on process-global
    /// state, libtest runs these in parallel threads of ONE process, and all
    /// three touch the same variable — so `bitmap_from_env_missing_var` could
    /// observe a value another test had just set, and either of the setters
    /// could have its value removed mid-test. The race was invisible because CI
    /// executed none of these tests: it ran exactly two of this crate's 21
    /// `--lib` tests, each named individually. The step that turns the rest on
    /// (suite-08 `Lib unit tests`) caught this on its first run, which is the
    /// argument for the step.
    ///
    /// Holding one lock across each whole test is what makes read-then-restore
    /// atomic with respect to the others; scoping it tighter would not.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Flag ID constants must match config/feature-flags.json.
    #[test]
    fn flag_id_constants_match_registry() {
        assert_eq!(MULTI_VAULT_ENABLED, 0, "MULTI_VAULT_ENABLED must be id=0");
        assert_eq!(
            PORTFOLIO_ROUTER_ENABLED, 1,
            "PORTFOLIO_ROUTER_ENABLED must be id=1"
        );
        assert_eq!(
            INDEXER_MULTI_VAULT_EVENTS, 2,
            "INDEXER_MULTI_VAULT_EVENTS must be id=2"
        );
    }

    #[test]
    fn is_enabled_bit_0() {
        // bitmap 0b001 — only flag 0 is on.
        assert!(is_enabled(MULTI_VAULT_ENABLED, 0b001));
        assert!(!is_enabled(PORTFOLIO_ROUTER_ENABLED, 0b001));
        assert!(!is_enabled(INDEXER_MULTI_VAULT_EVENTS, 0b001));
    }

    #[test]
    fn is_enabled_bit_1() {
        // bitmap 0b010 — only flag 1 is on.
        assert!(!is_enabled(MULTI_VAULT_ENABLED, 0b010));
        assert!(is_enabled(PORTFOLIO_ROUTER_ENABLED, 0b010));
        assert!(!is_enabled(INDEXER_MULTI_VAULT_EVENTS, 0b010));
    }

    #[test]
    fn is_enabled_bit_2() {
        // bitmap 0b100 — only flag 2 is on.
        assert!(!is_enabled(MULTI_VAULT_ENABLED, 0b100));
        assert!(!is_enabled(PORTFOLIO_ROUTER_ENABLED, 0b100));
        assert!(is_enabled(INDEXER_MULTI_VAULT_EVENTS, 0b100));
    }

    #[test]
    fn is_enabled_empty_bitmap() {
        assert!(!is_enabled(MULTI_VAULT_ENABLED, 0));
        assert!(!is_enabled(PORTFOLIO_ROUTER_ENABLED, 0));
        assert!(!is_enabled(INDEXER_MULTI_VAULT_EVENTS, 0));
    }

    #[test]
    fn is_enabled_all_flags() {
        let bitmap: u64 = 0b111;
        assert!(is_enabled(MULTI_VAULT_ENABLED, bitmap));
        assert!(is_enabled(PORTFOLIO_ROUTER_ENABLED, bitmap));
        assert!(is_enabled(INDEXER_MULTI_VAULT_EVENTS, bitmap));
    }

    #[test]
    fn bitmap_from_env_missing_var() {
        let _guard = ENV_LOCK.lock().expect("FEATURE_FLAGS lock");
        // Ensure the variable is not set in this test process.
        std::env::remove_var("FEATURE_FLAGS");
        assert_eq!(bitmap_from_env(), 0);
    }

    #[test]
    fn bitmap_from_env_valid_value() {
        let _guard = ENV_LOCK.lock().expect("FEATURE_FLAGS lock");
        std::env::set_var("FEATURE_FLAGS", "5");
        let bitmap = bitmap_from_env();
        std::env::remove_var("FEATURE_FLAGS");
        assert_eq!(bitmap, 5);
        // 5 = 0b101 → flags 0 and 2 enabled.
        assert!(is_enabled(MULTI_VAULT_ENABLED, bitmap));
        assert!(!is_enabled(PORTFOLIO_ROUTER_ENABLED, bitmap));
        assert!(is_enabled(INDEXER_MULTI_VAULT_EVENTS, bitmap));
    }

    #[test]
    fn bitmap_from_env_invalid_value() {
        let _guard = ENV_LOCK.lock().expect("FEATURE_FLAGS lock");
        std::env::set_var("FEATURE_FLAGS", "notanumber");
        let bitmap = bitmap_from_env();
        std::env::remove_var("FEATURE_FLAGS");
        assert_eq!(bitmap, 0);
    }
}
