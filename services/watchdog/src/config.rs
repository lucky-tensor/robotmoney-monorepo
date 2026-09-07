//! Watchdog threshold configuration.
//!
//! Canonical: docs/technical/security-model.md §9 — Automated watchdog thresholds.
//!
//! Configuration is loaded from a TOML file (default: `services/watchdog/config.toml`).
//! A missing or zero threshold is a fatal error at startup — per the acceptance criterion
//! in issue #658, the service must not run with an unconfigured safety envelope.
//!
//! # File layout
//!
//! ```toml
//! [global]
//! per_block_mint_limit_usdc   = "500000"   # USDC (6 decimals: 500 000 USDC per block)
//! per_hour_mint_limit_usdc    = "2000000"  # USDC (6 decimals: 2 000 000 USDC per hour)
//! per_block_burn_limit_usdc   = "500000"
//! per_hour_burn_limit_usdc    = "2000000"
//!
//! [action]
//! # "pause" → call gateway.pause() via funded PAUSER_ROLE key
//! # "alert" → dispatch structured JSON alert to webhook_url
//! # "pause_and_alert" → both
//! mode = "pause_and_alert"
//! # Required when mode contains "alert"
//! webhook_url = "https://events.pagerduty.com/v2/enqueue"
//! # Required when mode contains "pause"
//! gateway_rpc_url = "https://mainnet.base.org"
//! gateway_address = "0xDEADBEEF..."
//! pauser_private_key_hex = "0x..."          # PAUSER_ROLE key (funded for gas)
//!
//! [sla]
//! # Maximum seconds between breach detection and pause/alert dispatch.
//! # Recorded in the plan and asserted at runtime.
//! max_response_secs = 300   # 5 minutes
//!
//! # Optional per-vault overrides (keyed by lowercase hex address without 0x prefix).
//! # Vaults not listed here inherit the global limits.
//! [vault."abcdef1234567890abcdef1234567890abcdef12"]
//! per_block_mint_limit_usdc  = "100000"
//! per_hour_mint_limit_usdc   = "400000"
//! per_block_burn_limit_usdc  = "100000"
//! per_hour_burn_limit_usdc   = "400000"
//! ```
//!
//! # Pauser key delivery and lifetime
//!
//! The PAUSER_ROLE key may be delivered either by the TOML literal above (local
//! dev) or by the [`PAUSER_KEY_ENV`] environment variable, which wins when both
//! are set. Whichever source is used, [`Config::from_file`] stores it in a
//! [`PauserKeyHex`] — a wrapper that is **not** `Clone` and whose `Debug` is
//! redacting — and [`Config::take_pauser_signing_key`] consumes it exactly once
//! at startup, deriving the [`PauserSigningKey`] the pause path needs and
//! zeroizing the hex text as the wrapper drops. After that call the field is
//! `None`, so no later code path can read a raw pauser secret out of `Config`.
//!
//! What this does and does not buy: it bounds how long the *hex text* lives in
//! this process's own long-lived state, and it removes the derived-`Clone` and
//! derived-`Debug` paths that would have copied or printed it. It is **not** a
//! claim that no bytes of the key remain anywhere in memory — the TOML file
//! read, `String` growth/reallocation, and the environment block itself
//! (`/proc/self/environ`, inherited by children) can each retain copies that
//! this module cannot reach.

use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use zeroize::Zeroizing;

use crate::pause::PauserSigningKey;
use crate::receipt_liveness::ReceiptLivenessConfig;
use crate::WatchdogError;

/// Environment variable that supplies the PAUSER_ROLE private key (hex, with or
/// without a `0x` prefix). Takes precedence over the TOML literal.
pub const PAUSER_KEY_ENV: &str = "WATCHDOG_PAUSER_KEY_HEX";

/// Hex text of the PAUSER_ROLE private key, held only until the startup
/// extraction consumes it.
///
/// Deliberately **not** `Clone`: a derived `Clone` on the containing config
/// would copy the secret into a fresh allocation that nothing zeroizes. Its
/// `Debug` is redacting, so `{:?}`-printing any struct that contains it (or
/// logging that struct through `tracing`) cannot leak the key. The inner
/// [`Zeroizing`] wipes the string's current allocation on drop.
pub struct PauserKeyHex(Zeroizing<String>);

impl PauserKeyHex {
    /// Wrap raw hex key text. The caller's `String` is moved in, not copied.
    pub fn new(hex: String) -> Self {
        Self(Zeroizing::new(hex))
    }

    /// Borrow the hex text. Private to this module: the only supported consumer
    /// is [`Config::take_pauser_signing_key`].
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PauserKeyHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PauserKeyHex(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for PauserKeyHex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

/// Top-level configuration struct loaded from the TOML file.
///
/// Not `Clone`: it transitively holds the pauser key until startup consumes it,
/// and a clone would duplicate that secret into an allocation nothing zeroizes.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Global mint/burn volume thresholds.
    pub global: GlobalThresholds,
    /// Action to take on threshold breach.
    pub action: ActionConfig,
    /// SLA configuration.
    pub sla: SlaConfig,
    /// Optional per-vault overrides keyed by lowercase hex vault address (no `0x` prefix).
    #[serde(default)]
    pub vault: HashMap<String, VaultThresholds>,
    /// Consensus-receipt anchoring-gap monitor (issue #1247 task 4.13).
    /// Absent means disabled, so pre-existing configs keep parsing unchanged.
    #[serde(default)]
    pub consensus_receipts: ReceiptLivenessConfig,
}

/// Global per-block and per-hour mint/burn volume limits (USDC units, 6-decimal integer strings).
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalThresholds {
    /// Maximum total mint (deposit) volume per block across all vaults, in USDC base units.
    pub per_block_mint_limit_usdc: String,
    /// Maximum total mint (deposit) volume per rolling hour across all vaults, in USDC base units.
    pub per_hour_mint_limit_usdc: String,
    /// Maximum total burn (withdrawal) volume per block across all vaults, in USDC base units.
    pub per_block_burn_limit_usdc: String,
    /// Maximum total burn (withdrawal) volume per rolling hour across all vaults, in USDC base units.
    pub per_hour_burn_limit_usdc: String,
}

/// Per-vault threshold overrides.  Vaults not listed inherit the global limits.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultThresholds {
    /// Per-block mint limit override, in USDC base units.
    pub per_block_mint_limit_usdc: Option<String>,
    /// Per-hour mint limit override, in USDC base units.
    pub per_hour_mint_limit_usdc: Option<String>,
    /// Per-block burn limit override, in USDC base units.
    pub per_block_burn_limit_usdc: Option<String>,
    /// Per-hour burn limit override, in USDC base units.
    pub per_hour_burn_limit_usdc: Option<String>,
}

/// Action to take when a threshold is breached.
///
/// Not `Clone` — see [`PauserKeyHex`]. `Debug` is safe to derive only because
/// `PauserKeyHex`'s own `Debug` redacts.
#[derive(Debug, Deserialize)]
pub struct ActionConfig {
    /// Breach response mode: `"pause"`, `"alert"`, or `"pause_and_alert"`.
    pub mode: ActionMode,
    /// Webhook URL for structured JSON alerts (required when `mode` contains `"alert"`).
    pub webhook_url: Option<String>,
    /// JSON-RPC endpoint for the gateway chain (required when `mode` contains `"pause"`).
    pub gateway_rpc_url: Option<String>,
    /// Deployed gateway contract address (required when `mode` contains `"pause"`).
    pub gateway_address: Option<String>,
    /// Hex-encoded private key for the PAUSER_ROLE account (required when `mode`
    /// contains `"pause"`, from this field or from [`PAUSER_KEY_ENV`]).
    ///
    /// Consumed exactly once by [`Config::take_pauser_signing_key`] at startup
    /// and `None` from then on — the watchdog does not keep the raw key for the
    /// life of the process. Use [`ActionConfig::has_pauser_key`] to test for
    /// presence without touching the secret.
    pub pauser_private_key_hex: Option<PauserKeyHex>,
    /// Gas-price bump applied over the network `eth_gasPrice` when submitting a
    /// pause tx, in basis points (e.g. `1500` = +15%). Lets a retried pause
    /// replace a stuck same-nonce tx by out-bidding it (scan finding WD-5).
    /// Defaults to [`DEFAULT_PAUSE_FEE_BUMP_BPS`] when absent.
    #[serde(default = "default_pause_fee_bump_bps")]
    pub pause_fee_bump_bps: u64,
}

impl ActionConfig {
    /// True while a pauser key is still held (i.e. before the startup
    /// extraction consumed it). Reports presence without exposing the secret.
    pub fn has_pauser_key(&self) -> bool {
        self.pauser_private_key_hex.is_some()
    }
}

/// Default pause-tx fee bump: +15% over the network gas price, enough to replace
/// a stuck under-priced same-nonce tx on Base under typical conditions.
pub const DEFAULT_PAUSE_FEE_BUMP_BPS: u64 = 1500;

fn default_pause_fee_bump_bps() -> u64 {
    DEFAULT_PAUSE_FEE_BUMP_BPS
}

/// Response mode on threshold breach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionMode {
    /// Call `gateway.pause()` via a funded PAUSER_ROLE key.
    Pause,
    /// Dispatch a structured JSON alert to the configured webhook/PagerDuty endpoint.
    Alert,
    /// Both pause and alert.
    PauseAndAlert,
}

impl ActionMode {
    /// True if this mode involves calling `gateway.pause()`.
    pub fn includes_pause(self) -> bool {
        matches!(self, ActionMode::Pause | ActionMode::PauseAndAlert)
    }

    /// True if this mode involves dispatching a webhook alert.
    pub fn includes_alert(self) -> bool {
        matches!(self, ActionMode::Alert | ActionMode::PauseAndAlert)
    }
}

/// SLA configuration — maximum response time from breach detection.
#[derive(Debug, Clone, Deserialize)]
pub struct SlaConfig {
    /// Maximum seconds between breach detection and pause/alert dispatch.
    /// Recorded in the plan (issue #658). Must be non-zero.
    pub max_response_secs: u64,
}

impl Config {
    /// Load and validate a [`Config`] from a TOML file.
    ///
    /// Returns [`WatchdogError::Config`] if the file cannot be read, parsed, or
    /// fails any threshold validation (missing or zero limits).
    /// The file may carry the pauser key as a TOML literal; [`PAUSER_KEY_ENV`]
    /// overrides it when set. The file text is held in a [`Zeroizing`] buffer so
    /// a key embedded in it does not linger in the read buffer after parsing.
    pub fn from_file(path: &Path) -> Result<Self, WatchdogError> {
        let raw = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
            WatchdogError::Config(format!("cannot read config file {}: {e}", path.display()))
        })?);
        let mut cfg: Config = toml::from_str(&raw).map_err(|e| {
            WatchdogError::Config(format!("config parse error in {}: {e}", path.display()))
        })?;
        cfg.apply_pauser_key_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Overlay [`PAUSER_KEY_ENV`] onto the parsed config when it is set to a
    /// non-empty value, so deployments can deliver the key out-of-band while the
    /// TOML literal keeps working unchanged for local dev.
    ///
    /// An empty or whitespace-only value is ignored rather than treated as a
    /// key: an accidentally-blank env var must not silently erase a configured
    /// TOML key and disarm the pause path.
    pub fn apply_pauser_key_env(&mut self) {
        if let Ok(value) = std::env::var(PAUSER_KEY_ENV) {
            if !value.trim().is_empty() {
                // `value` is moved (not copied) into the zeroizing wrapper. The
                // copy in the process environment block is outside our reach.
                self.action.pauser_private_key_hex = Some(PauserKeyHex::new(value));
            }
        }
    }

    /// Consume the configured pauser key **exactly once** and return the signing
    /// state the pause path needs.
    ///
    /// Returns `Ok(None)` when no key is configured (valid for alert-only modes,
    /// and what a second call returns after the first has taken the key). The
    /// hex text is zeroized as the taken [`PauserKeyHex`] drops at the end of
    /// this call; only the derived `k256` signing key — which zeroizes its own
    /// scalar on drop — outlives it.
    pub fn take_pauser_signing_key(&mut self) -> Result<Option<PauserSigningKey>, WatchdogError> {
        match self.action.pauser_private_key_hex.take() {
            Some(key_hex) => {
                let signer = PauserSigningKey::from_hex(key_hex.expose())?;
                Ok(Some(signer))
            }
            None => Ok(None),
        }
    }

    /// Validate that all required threshold and action fields are present and non-zero.
    ///
    /// A missing or zero threshold is a fatal error: running the watchdog without a
    /// configured safety envelope would silently disable the control.
    pub fn validate(&self) -> Result<(), WatchdogError> {
        // A zero receipt cadence would page on every poll; refuse it rather
        // than silently disable the control (issue #1247 task 4.13).
        self.consensus_receipts.validate()?;

        // Validate global thresholds.
        validate_threshold(
            &self.global.per_block_mint_limit_usdc,
            "global.per_block_mint_limit_usdc",
        )?;
        validate_threshold(
            &self.global.per_hour_mint_limit_usdc,
            "global.per_hour_mint_limit_usdc",
        )?;
        validate_threshold(
            &self.global.per_block_burn_limit_usdc,
            "global.per_block_burn_limit_usdc",
        )?;
        validate_threshold(
            &self.global.per_hour_burn_limit_usdc,
            "global.per_hour_burn_limit_usdc",
        )?;

        // Validate per-vault overrides.
        for (vault_addr, vt) in &self.vault {
            if let Some(ref v) = vt.per_block_mint_limit_usdc {
                validate_threshold(v, &format!("vault.{vault_addr}.per_block_mint_limit_usdc"))?;
            }
            if let Some(ref v) = vt.per_hour_mint_limit_usdc {
                validate_threshold(v, &format!("vault.{vault_addr}.per_hour_mint_limit_usdc"))?;
            }
            if let Some(ref v) = vt.per_block_burn_limit_usdc {
                validate_threshold(v, &format!("vault.{vault_addr}.per_block_burn_limit_usdc"))?;
            }
            if let Some(ref v) = vt.per_hour_burn_limit_usdc {
                validate_threshold(v, &format!("vault.{vault_addr}.per_hour_burn_limit_usdc"))?;
            }
        }

        // Validate SLA.
        if self.sla.max_response_secs == 0 {
            return Err(WatchdogError::Config(
                "sla.max_response_secs must be non-zero".to_owned(),
            ));
        }

        // Validate action config.
        if self.action.mode.includes_alert() && self.action.webhook_url.is_none() {
            return Err(WatchdogError::Config(
                "action.webhook_url is required when action.mode includes alert".to_owned(),
            ));
        }
        if self.action.mode.includes_pause() {
            if self.action.gateway_rpc_url.is_none() {
                return Err(WatchdogError::Config(
                    "action.gateway_rpc_url is required when action.mode includes pause".to_owned(),
                ));
            }
            if self.action.gateway_address.is_none() {
                return Err(WatchdogError::Config(
                    "action.gateway_address is required when action.mode includes pause".to_owned(),
                ));
            }
            if !self.action.has_pauser_key() {
                return Err(WatchdogError::Config(format!(
                    "action.pauser_private_key_hex (or {PAUSER_KEY_ENV}) is required \
                     when action.mode includes pause"
                )));
            }
        }

        Ok(())
    }

    /// Return the effective per-block mint limit for a given vault address (hex, no prefix).
    ///
    /// Falls back to the global limit if no per-vault override is configured.
    pub fn per_block_mint_limit(&self, vault_hex: &str) -> u128 {
        self.vault
            .get(vault_hex)
            .and_then(|v| v.per_block_mint_limit_usdc.as_deref())
            .unwrap_or(&self.global.per_block_mint_limit_usdc)
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the effective per-hour mint limit for a given vault address (hex, no prefix).
    pub fn per_hour_mint_limit(&self, vault_hex: &str) -> u128 {
        self.vault
            .get(vault_hex)
            .and_then(|v| v.per_hour_mint_limit_usdc.as_deref())
            .unwrap_or(&self.global.per_hour_mint_limit_usdc)
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the effective per-block burn limit for a given vault address (hex, no prefix).
    pub fn per_block_burn_limit(&self, vault_hex: &str) -> u128 {
        self.vault
            .get(vault_hex)
            .and_then(|v| v.per_block_burn_limit_usdc.as_deref())
            .unwrap_or(&self.global.per_block_burn_limit_usdc)
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the effective per-hour burn limit for a given vault address (hex, no prefix).
    pub fn per_hour_burn_limit(&self, vault_hex: &str) -> u128 {
        self.vault
            .get(vault_hex)
            .and_then(|v| v.per_hour_burn_limit_usdc.as_deref())
            .unwrap_or(&self.global.per_hour_burn_limit_usdc)
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the global per-block mint limit.
    pub fn global_per_block_mint_limit(&self) -> u128 {
        self.global
            .per_block_mint_limit_usdc
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the global per-hour mint limit.
    pub fn global_per_hour_mint_limit(&self) -> u128 {
        self.global
            .per_hour_mint_limit_usdc
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the global per-block burn limit.
    pub fn global_per_block_burn_limit(&self) -> u128 {
        self.global
            .per_block_burn_limit_usdc
            .parse::<u128>()
            .unwrap_or(0)
    }

    /// Return the global per-hour burn limit.
    pub fn global_per_hour_burn_limit(&self) -> u128 {
        self.global
            .per_hour_burn_limit_usdc
            .parse::<u128>()
            .unwrap_or(0)
    }
}

/// Validate that a threshold string is present, parseable as u128, and non-zero.
fn validate_threshold(value: &str, field: &str) -> Result<(), WatchdogError> {
    match value.trim().parse::<u128>() {
        Ok(0) => Err(WatchdogError::Config(format!(
            "{field} must be non-zero (got 0)"
        ))),
        Ok(_) => Ok(()),
        Err(_) => Err(WatchdogError::Config(format!(
            "{field} is not a valid integer: {value:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// A valid (non-zero, in-range) secp256k1 scalar for tests. Not a real key.
    const TEST_KEY_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER_KEY_HEX: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// `std::env` is process-global, so the env-var tests must not run
    /// concurrently with each other. Serialise them behind one lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Write `raw` to a temp file and load it through the real
    /// [`Config::from_file`] path (which is where env overlay happens).
    fn load_from_temp_file(raw: &str) -> Result<Config, WatchdogError> {
        let path = std::env::temp_dir().join(format!(
            "watchdog-config-test-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(raw.as_bytes()).unwrap();
        drop(f);
        let result = Config::from_file(&path);
        let _ = std::fs::remove_file(&path);
        result
    }

    fn pause_mode_toml(pauser_line: &str) -> String {
        format!(
            r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "pause"
gateway_rpc_url = "https://rpc.example.com"
gateway_address = "0x000000000000000000000000000000000000dEaD"
{pauser_line}

[sla]
max_response_secs = 300
"#
        )
    }

    fn make_valid_config_toml() -> String {
        r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 300
"#
        .to_owned()
    }

    #[test]
    fn valid_config_parses_and_validates() {
        let raw = make_valid_config_toml();
        let cfg: Config = toml::from_str(&raw).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn config_missing_threshold_is_fatal_zero_block_mint() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "0"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 300
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            WatchdogError::Config(msg) => {
                assert!(
                    msg.contains("per_block_mint_limit_usdc"),
                    "error must name the bad field; got: {msg}"
                );
            }
            other => panic!("expected WatchdogError::Config, got: {other}"),
        }
    }

    #[test]
    fn config_missing_threshold_is_fatal_missing_hour_burn() {
        // Missing field entirely → toml parse error, not validation error.
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
# per_hour_burn_limit_usdc is intentionally absent

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 300
"#;
        let result: Result<Config, _> = toml::from_str(raw);
        assert!(result.is_err(), "missing field must fail TOML parse");
    }

    #[test]
    fn config_missing_threshold_is_fatal_non_integer() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "not_a_number"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 300
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            WatchdogError::Config(msg) => {
                assert!(msg.contains("per_block_mint_limit_usdc"), "got: {msg}");
            }
            other => panic!("expected WatchdogError::Config, got: {other}"),
        }
    }

    #[test]
    fn alert_mode_requires_webhook_url() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
# webhook_url absent

[sla]
max_response_secs = 300
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            WatchdogError::Config(msg) => {
                assert!(msg.contains("webhook_url"), "got: {msg}");
            }
            other => panic!("expected WatchdogError::Config, got: {other}"),
        }
    }

    #[test]
    fn pause_mode_requires_gateway_fields() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "pause"
# No gateway fields

[sla]
max_response_secs = 300
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_sla_is_fatal() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 0
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err();
        match err {
            WatchdogError::Config(msg) => {
                assert!(msg.contains("max_response_secs"), "got: {msg}");
            }
            other => panic!("expected WatchdogError::Config, got: {other}"),
        }
    }

    #[test]
    fn global_limit_accessors_parse_correctly() {
        let raw = make_valid_config_toml();
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg.global_per_block_mint_limit(), 500_000);
        assert_eq!(cfg.global_per_hour_mint_limit(), 2_000_000);
        assert_eq!(cfg.global_per_block_burn_limit(), 500_000);
        assert_eq!(cfg.global_per_hour_burn_limit(), 2_000_000);
    }

    #[test]
    fn per_vault_override_is_respected() {
        let raw = r#"
[global]
per_block_mint_limit_usdc   = "500000"
per_hour_mint_limit_usdc    = "2000000"
per_block_burn_limit_usdc   = "500000"
per_hour_burn_limit_usdc    = "2000000"

[action]
mode = "alert"
webhook_url = "https://events.example.com/alert"

[sla]
max_response_secs = 300

[vault."aabbccdd00112233aabbccdd00112233aabbccdd"]
per_block_mint_limit_usdc = "100000"
per_hour_mint_limit_usdc  = "400000"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        let vault = "aabbccdd00112233aabbccdd00112233aabbccdd";
        assert_eq!(cfg.per_block_mint_limit(vault), 100_000);
        assert_eq!(cfg.per_hour_mint_limit(vault), 400_000);
        // burn falls back to global
        assert_eq!(cfg.per_block_burn_limit(vault), 500_000);
        assert_eq!(cfg.per_hour_burn_limit(vault), 2_000_000);
    }

    // ---- Pauser key delivery and single-use extraction (issue #1357) --------

    /// The env var is a first-class source: a config file with no pauser
    /// literal still starts, and the key it yields is the env var's.
    #[test]
    fn pauser_key_env_var_is_honored() {
        let _guard = env_lock();
        std::env::set_var(PAUSER_KEY_ENV, TEST_KEY_HEX);
        let result = load_from_temp_file(&pause_mode_toml("# no TOML literal"));
        std::env::remove_var(PAUSER_KEY_ENV);

        let mut cfg = result.expect("env-supplied pauser key must satisfy pause-mode validation");
        assert!(
            cfg.action.has_pauser_key(),
            "env key must land in the config"
        );
        let signer = cfg
            .take_pauser_signing_key()
            .expect("env key must derive")
            .expect("a key was configured");
        let expected = PauserSigningKey::from_hex(TEST_KEY_HEX).unwrap();
        assert_eq!(signer.address(), expected.address());
    }

    /// Local dev is unchanged: with the env var absent, the TOML literal is
    /// still the source of the key.
    #[test]
    fn toml_literal_still_works_without_env_var() {
        let _guard = env_lock();
        std::env::remove_var(PAUSER_KEY_ENV);
        let mut cfg = load_from_temp_file(&pause_mode_toml(&format!(
            "pauser_private_key_hex = \"0x{TEST_KEY_HEX}\""
        )))
        .expect("TOML literal must still satisfy pause-mode validation");
        let signer = cfg
            .take_pauser_signing_key()
            .expect("TOML key must derive")
            .expect("a key was configured");
        let expected = PauserSigningKey::from_hex(TEST_KEY_HEX).unwrap();
        assert_eq!(signer.address(), expected.address());
    }

    /// When both sources are present the env var wins, so a deployment can
    /// override a literal baked into a shipped config file.
    #[test]
    fn env_var_overrides_toml_literal() {
        let _guard = env_lock();
        std::env::set_var(PAUSER_KEY_ENV, OTHER_KEY_HEX);
        let result = load_from_temp_file(&pause_mode_toml(&format!(
            "pauser_private_key_hex = \"0x{TEST_KEY_HEX}\""
        )));
        std::env::remove_var(PAUSER_KEY_ENV);

        let mut cfg = result.expect("config with both sources must validate");
        let signer = cfg.take_pauser_signing_key().unwrap().unwrap();
        let from_env = PauserSigningKey::from_hex(OTHER_KEY_HEX).unwrap();
        let from_toml = PauserSigningKey::from_hex(TEST_KEY_HEX).unwrap();
        assert_eq!(signer.address(), from_env.address(), "env var must win");
        assert_ne!(signer.address(), from_toml.address());
    }

    /// A blank env var must not disarm a working TOML-configured pause path.
    #[test]
    fn blank_env_var_does_not_erase_toml_literal() {
        let _guard = env_lock();
        std::env::set_var(PAUSER_KEY_ENV, "   ");
        let result = load_from_temp_file(&pause_mode_toml(&format!(
            "pauser_private_key_hex = \"0x{TEST_KEY_HEX}\""
        )));
        std::env::remove_var(PAUSER_KEY_ENV);

        let mut cfg = result.expect("blank env var must leave the TOML literal in place");
        let signer = cfg.take_pauser_signing_key().unwrap().unwrap();
        assert_eq!(
            signer.address(),
            PauserSigningKey::from_hex(TEST_KEY_HEX).unwrap().address()
        );
    }

    /// Pause mode with neither source configured is still a fatal startup
    /// error — the env var is an additional source, not an escape hatch.
    #[test]
    fn pause_mode_fails_when_neither_source_provides_key() {
        let _guard = env_lock();
        std::env::remove_var(PAUSER_KEY_ENV);
        let err = load_from_temp_file(&pause_mode_toml("# no TOML literal"))
            .expect_err("pause mode without any key source must fail startup");
        match err {
            WatchdogError::Config(msg) => {
                assert!(
                    msg.contains("pauser"),
                    "error must name the key; got: {msg}"
                );
            }
            other => panic!("expected WatchdogError::Config, got: {other}"),
        }
    }

    /// AC: the raw secret does not outlive its first use in `Config`. After one
    /// extraction the field is empty and a second attempt reads no raw secret
    /// back out of `Config`.
    ///
    /// This is the mechanically checkable half of the claim: it proves the
    /// secret is gone from *this struct*, not that no bytes of it remain
    /// anywhere in process memory (which is not automatable — see the module
    /// docs).
    #[test]
    fn pauser_key_is_consumed_by_first_extraction() {
        let _guard = env_lock();
        std::env::remove_var(PAUSER_KEY_ENV);
        let mut cfg = load_from_temp_file(&pause_mode_toml(&format!(
            "pauser_private_key_hex = \"0x{TEST_KEY_HEX}\""
        )))
        .unwrap();

        assert!(cfg.action.has_pauser_key(), "key present before extraction");
        let first = cfg.take_pauser_signing_key().unwrap();
        assert!(first.is_some(), "first extraction yields the signing key");

        // The field that used to hold the plain-text secret is now empty…
        assert!(
            !cfg.action.has_pauser_key(),
            "the raw secret must not outlive its first use in Config"
        );
        assert!(
            cfg.action.pauser_private_key_hex.is_none(),
            "the field itself must be None after extraction"
        );
        // …and a second extraction cannot read a raw secret from Config again.
        let second = cfg.take_pauser_signing_key().unwrap();
        assert!(
            second.is_none(),
            "a second extraction must find nothing left to extract"
        );
    }

    /// `Debug` on the config (directly or via `tracing`'s `?field`) must never
    /// print the key — the derive would have, before `PauserKeyHex` redacted.
    #[test]
    fn debug_output_never_contains_the_pauser_key() {
        let _guard = env_lock();
        std::env::remove_var(PAUSER_KEY_ENV);
        let cfg = load_from_temp_file(&pause_mode_toml(&format!(
            "pauser_private_key_hex = \"0x{TEST_KEY_HEX}\""
        )))
        .unwrap();

        for rendered in [format!("{cfg:?}"), format!("{:?}", cfg.action)] {
            assert!(
                !rendered.contains(TEST_KEY_HEX),
                "Debug output leaked the pauser key: {rendered}"
            );
            assert!(
                rendered.contains("redacted"),
                "the redacted marker must show the field exists: {rendered}"
            );
        }
    }
}
