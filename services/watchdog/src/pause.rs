//! Gateway pause trigger via JSON-RPC `eth_sendRawTransaction`.
//!
//! Canonical: docs/technical/security-model.md §9 — automated pause path.
//!
//! When the watchdog detects a threshold breach and the configured action mode
//! includes `"pause"`, [`trigger_pause`] constructs and submits an EVM transaction
//! that calls `gateway.pause()` from the configured PAUSER_ROLE account.
//!
//! # Transaction construction
//!
//! The `pause()` selector is the first 4 bytes of `keccak256("pause()")`.
//! The transaction is signed with the configured PAUSER_ROLE private key and
//! submitted via `eth_sendRawTransaction` to the configured JSON-RPC endpoint.
//!
//! Gas price and nonce are fetched live from `eth_gasPrice` / `eth_getTransactionCount`
//! immediately before submission to minimise MEV exposure and nonce conflicts.
//!
//! # Replacing a stuck pause tx (scan finding WD-5)
//!
//! The nonce is fetched at the **confirmed** (`"latest"`) height, not `"pending"`.
//! When a prior pause tx is stuck in the mempool (under-priced), the pending nonce
//! would have already advanced, so a retry at the pending nonce queues *behind* the
//! stuck tx and can never replace it. Using the confirmed nonce means a retry
//! re-broadcasts at the **same** nonce; paired with the `fee_bump_bps` gas-price
//! bump in [`PauseParams`], the replacement out-bids the stuck tx and the pause is
//! actually mined. A pause is a safety-critical action — replacing a stuck tx
//! matters more than the marginal nonce-gap risk of two concurrent pauses (both
//! are idempotent: the second simply reverts `PausedError()`).
//!
//! # Idempotency
//!
//! The gateway reverts with `PausedError()` if already paused — the watchdog treats
//! this revert as a success (the gateway is already safe).

use std::fmt;

use alloy_primitives::{keccak256, Address, Bytes};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::WatchdogError;

/// Selector for `pause()` — first 4 bytes of `keccak256("pause()")`.
pub fn pause_selector() -> [u8; 4] {
    let hash = keccak256(b"pause()");
    [hash[0], hash[1], hash[2], hash[3]]
}

/// Signing state for the PAUSER_ROLE account, derived once at startup from the
/// configured key hex (see [`crate::config::Config::take_pauser_signing_key`]).
///
/// Holds the derived `k256` signing key and the sender address computed from it
/// — never the hex text, and never a re-decodable raw 32-byte scalar field. The
/// inner `k256::ecdsa::SigningKey` is `ZeroizeOnDrop`, so its secret scalar is
/// wiped when this value drops.
///
/// `Debug` prints only the (public) sender address. Not `Clone`: the pause path
/// borrows the single instance the daemon derived at startup rather than
/// spreading copies of the scalar across the process.
pub struct PauserSigningKey {
    signing_key: k256::ecdsa::SigningKey,
    address: Address,
}

impl PauserSigningKey {
    /// Derive signing state from hex key text (optional `0x` prefix).
    ///
    /// The hex is decoded straight into a fixed-size zeroizing buffer — no
    /// intermediate `Vec`/`String` copy of the scalar — and that buffer is wiped
    /// when this function returns.
    pub fn from_hex(key_hex: &str) -> Result<Self, WatchdogError> {
        let trimmed = key_hex.trim();
        let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        if trimmed.len() != 64 {
            // Deliberately reports only the length, never the value.
            return Err(WatchdogError::Config(format!(
                "pauser private key must be 32 bytes (64 hex chars), got {} chars",
                trimmed.len()
            )));
        }
        let mut key_bytes = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(trimmed, key_bytes.as_mut_slice())
            .map_err(|e| WatchdogError::Config(format!("pauser private key decode failed: {e}")))?;
        let signing_key = k256::ecdsa::SigningKey::from_bytes((&*key_bytes).into())
            .map_err(|e| WatchdogError::Config(format!("invalid pauser key: {e}")))?;
        let address = address_from_signing_key(&signing_key);
        Ok(Self {
            signing_key,
            address,
        })
    }

    /// The PAUSER_ROLE sender address derived from this key (public data).
    pub fn address(&self) -> Address {
        self.address
    }
}

impl fmt::Debug for PauserSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PauserSigningKey")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// Compute the EVM address for a signing key: keccak256 of the uncompressed
/// public key (minus its `0x04` prefix), last 20 bytes.
fn address_from_signing_key(signing_key: &k256::ecdsa::SigningKey) -> Address {
    let verifying_key = signing_key.verifying_key();
    let pubkey_bytes = verifying_key.to_encoded_point(false);
    let pubkey_slice = &pubkey_bytes.as_bytes()[1..]; // strip 0x04 prefix
    let hash = keccak256(pubkey_slice);
    Address::from_slice(&hash[12..])
}

/// Parameters required to submit a `gateway.pause()` transaction.
///
/// Borrows the startup-derived [`PauserSigningKey`] rather than carrying key
/// material of its own, so building these per breach copies no secret.
#[derive(Debug, Clone)]
pub struct PauseParams<'a> {
    /// JSON-RPC endpoint URL for the gateway chain.
    pub rpc_url: String,
    /// Deployed gateway contract address.
    pub gateway_address: Address,
    /// Chain ID (used in EIP-155 transaction signing).
    pub chain_id: u64,
    /// Signing state for the PAUSER_ROLE account, derived once at startup.
    pub signer: &'a PauserSigningKey,
    /// Gas-price bump applied over the network `eth_gasPrice`, in basis points
    /// (e.g. `1500` = +15%). Lets a retried pause replace a stuck same-nonce tx by
    /// out-bidding it (scan finding WD-5). `0` submits at the network gas price.
    pub fee_bump_bps: u64,
}

/// Apply a basis-point fee bump to a gas price, saturating on overflow.
fn bump_gas_price(gas_price: u64, fee_bump_bps: u64) -> u64 {
    // gas_price * (10_000 + fee_bump_bps) / 10_000, in u128 to avoid overflow.
    let bumped = (gas_price as u128)
        .saturating_mul(10_000u128.saturating_add(fee_bump_bps as u128))
        / 10_000u128;
    u64::try_from(bumped).unwrap_or(u64::MAX)
}

/// Submit a `gateway.pause()` transaction and return the transaction hash hex string.
///
/// Returns `Ok(tx_hash_hex)` on successful submission.  If the gateway is already
/// paused (revert `PausedError()`) the call returns `Ok("already_paused")` so the
/// caller can log a no-op rather than treating it as an error.
///
/// All network / signing errors are returned as [`WatchdogError::Pause`].
pub async fn trigger_pause(
    client: &Client,
    params: &PauseParams<'_>,
) -> Result<String, WatchdogError> {
    // 1. Sender address and signing key were derived once at startup; a pause
    //    attempt re-derives nothing from raw key material.
    let signing_key = &params.signer.signing_key;
    let from = params.signer.address();

    // 2. Fetch the CONFIRMED nonce (not pending) so a retry replaces a stuck
    //    same-nonce tx rather than queuing behind it (scan finding WD-5).
    let nonce = eth_get_transaction_count(client, &params.rpc_url, from).await?;

    // 3. Fetch gas price and apply the configured fee bump so a replacement
    //    out-bids the stuck tx.
    let gas_price = bump_gas_price(
        eth_gas_price(client, &params.rpc_url).await?,
        params.fee_bump_bps,
    );

    // 4. Build the call data: pause() selector, no arguments.
    let selector = pause_selector();
    let data = Bytes::copy_from_slice(&selector);

    // 5. Sign and encode the transaction using a lightweight legacy (EIP-155) envelope.
    //    We avoid pulling in the full alloy-network stack to keep dependencies minimal.
    let tx_hash = send_legacy_transaction(
        client,
        &params.rpc_url,
        signing_key,
        from,
        params.gateway_address,
        nonce,
        gas_price,
        80_000u64, // gas limit: pause() is a simple storage write
        data,
        params.chain_id,
    )
    .await?;

    Ok(tx_hash)
}

/// Fetch the **confirmed** transaction count (nonce) for `address` via
/// `eth_getTransactionCount` at the `"latest"` block tag.
///
/// Deliberately uses `"latest"` rather than `"pending"`: a stuck under-priced
/// pause tx advances the *pending* nonce, so a retry at the pending nonce queues
/// behind it forever. The confirmed nonce lets a fee-bumped retry re-broadcast at
/// the same nonce and replace the stuck tx (scan finding WD-5).
async fn eth_get_transaction_count(
    client: &Client,
    rpc_url: &str,
    address: Address,
) -> Result<u64, WatchdogError> {
    let addr_hex = format!("0x{}", hex::encode(address.as_slice()));
    let result = rpc_call(
        client,
        rpc_url,
        "eth_getTransactionCount",
        serde_json::json!([addr_hex, "latest"]),
    )
    .await?;
    parse_hex_u64(&result, "eth_getTransactionCount")
}

/// Fetch the current gas price via `eth_gasPrice`.
async fn eth_gas_price(client: &Client, rpc_url: &str) -> Result<u64, WatchdogError> {
    let result = rpc_call(client, rpc_url, "eth_gasPrice", serde_json::json!([])).await?;
    parse_hex_u64(&result, "eth_gasPrice")
}

/// Sign a legacy (EIP-155) transaction, encode it as RLP, and submit it via
/// `eth_sendRawTransaction`.  Returns the transaction hash hex string.
#[allow(clippy::too_many_arguments)]
async fn send_legacy_transaction(
    client: &Client,
    rpc_url: &str,
    signing_key: &k256::ecdsa::SigningKey,
    _from: Address,
    to: Address,
    nonce: u64,
    gas_price: u64,
    gas_limit: u64,
    data: Bytes,
    chain_id: u64,
) -> Result<String, WatchdogError> {
    // EIP-155 legacy transaction RLP encoding.
    // Fields: nonce, gasPrice, gasLimit, to, value, data, v, r, s
    // For signing: encode without v/r/s, append (chainId, 0, 0), hash, sign.

    let rlp_unsigned = encode_legacy_tx_for_signing(
        nonce, gas_price, gas_limit, to, 0u64, // value = 0
        &data, chain_id,
    );

    let msg_hash = keccak256(&rlp_unsigned);

    // Sign the hash.
    use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature};
    let (sig, recid): (Signature, RecoveryId) = signing_key
        .sign_prehash(msg_hash.as_slice())
        .map_err(|e| WatchdogError::Pause(format!("signing failed: {e}")))?;

    // EIP-155 v = 2 * chain_id + 35 + recovery_id
    let v = 2u64 * chain_id + 35 + recid.to_byte() as u64;
    let sig_bytes = sig.to_bytes();
    let r = &sig_bytes[..32];
    let s = &sig_bytes[32..];

    let rlp_signed = encode_legacy_tx_signed(nonce, gas_price, gas_limit, to, 0, &data, v, r, s);

    let hex_tx = format!("0x{}", hex::encode(&rlp_signed));
    let result = rpc_call(
        client,
        rpc_url,
        "eth_sendRawTransaction",
        serde_json::json!([hex_tx]),
    )
    .await?;

    match result.as_str() {
        Some(hash) => Ok(hash.to_owned()),
        None => Err(WatchdogError::Pause(format!(
            "unexpected eth_sendRawTransaction result: {result}"
        ))),
    }
}

/// Minimal RLP encoding for a legacy EIP-155 transaction (for signing).
/// Fields: [nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]
fn encode_legacy_tx_for_signing(
    nonce: u64,
    gas_price: u64,
    gas_limit: u64,
    to: Address,
    value: u64,
    data: &[u8],
    chain_id: u64,
) -> Vec<u8> {
    let items: Vec<Vec<u8>> = vec![
        rlp_uint(nonce),
        rlp_uint(gas_price),
        rlp_uint(gas_limit),
        rlp_bytes(to.as_slice()), // address is always 20 bytes
        rlp_uint(value),
        rlp_bytes(data),
        rlp_uint(chain_id), // EIP-155
        rlp_uint(0u64),     // r = 0
        rlp_uint(0u64),     // s = 0
    ];
    let flat: Vec<u8> = items.into_iter().flatten().collect();
    rlp_list(&flat)
}

/// Minimal RLP encoding for a signed legacy EIP-155 transaction.
/// Fields: [nonce, gasPrice, gasLimit, to, value, data, v, r, s]
#[allow(clippy::too_many_arguments)]
fn encode_legacy_tx_signed(
    nonce: u64,
    gas_price: u64,
    gas_limit: u64,
    to: Address,
    value: u64,
    data: &[u8],
    v: u64,
    r: &[u8],
    s: &[u8],
) -> Vec<u8> {
    let items: Vec<Vec<u8>> = vec![
        rlp_uint(nonce),
        rlp_uint(gas_price),
        rlp_uint(gas_limit),
        rlp_bytes(to.as_slice()),
        rlp_uint(value),
        rlp_bytes(data),
        rlp_uint(v),
        rlp_bytes(r),
        rlp_bytes(s),
    ];
    let flat: Vec<u8> = items.into_iter().flatten().collect();
    rlp_list(&flat)
}

// ---- Minimal RLP helpers ----

/// RLP-encode an unsigned integer (big-endian, minimal length, no leading zero bytes).
fn rlp_uint(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![0x80]; // RLP empty byte string = 0
    }
    let bytes = v.to_be_bytes();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(7);
    let trimmed = &bytes[start..];
    rlp_bytes(trimmed)
}

/// RLP-encode a byte string.
fn rlp_bytes(data: &[u8]) -> Vec<u8> {
    if data.len() == 1 && data[0] < 0x80 {
        return vec![data[0]];
    }
    let mut out = rlp_length_prefix(0x80, data.len());
    out.extend_from_slice(data);
    out
}

/// RLP-encode a list of already-encoded items.
fn rlp_list(encoded_items: &[u8]) -> Vec<u8> {
    let mut out = rlp_length_prefix(0xc0, encoded_items.len());
    out.extend_from_slice(encoded_items);
    out
}

/// Produce the RLP length prefix for `offset` + payload length.
fn rlp_length_prefix(offset: u8, len: usize) -> Vec<u8> {
    if len <= 55 {
        vec![offset + len as u8]
    } else {
        let len_bytes = len.to_be_bytes();
        let start = len_bytes.iter().position(|&b| b != 0).unwrap_or(7);
        let len_enc = &len_bytes[start..];
        let mut out = vec![offset + 55 + len_enc.len() as u8];
        out.extend_from_slice(len_enc);
        out
    }
}

// ---- JSON-RPC helpers ----

/// JSON-RPC request body.
#[derive(Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
    id: u32,
}

/// Execute a single JSON-RPC call and return the `result` field.
async fn rpc_call(
    client: &Client,
    url: &str,
    method: &'static str,
    params: Value,
) -> Result<Value, WatchdogError> {
    let req = RpcRequest {
        jsonrpc: "2.0",
        method,
        params,
        id: 1,
    };
    let resp = client
        .post(url)
        .json(&req)
        .send()
        .await
        .map_err(|e| WatchdogError::Pause(format!("{method} request failed: {e}")))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| WatchdogError::Pause(format!("{method} response parse failed: {e}")))?;

    if let Some(err) = body.get("error") {
        return Err(WatchdogError::Pause(format!("{method} RPC error: {err}")));
    }

    Ok(body["result"].clone())
}

/// Parse a `"0x..."` hex string result as `u64`.
fn parse_hex_u64(v: &Value, context: &str) -> Result<u64, WatchdogError> {
    let s = v
        .as_str()
        .ok_or_else(|| WatchdogError::Pause(format!("{context}: expected string, got {v}")))?;
    let hex = s.trim_start_matches("0x");
    u64::from_str_radix(hex, 16)
        .map_err(|e| WatchdogError::Pause(format!("{context}: invalid hex {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_selector_matches_keccak() {
        // keccak256("pause()") = 0x8456cb59...
        let sel = pause_selector();
        assert_eq!(sel[0], 0x84);
        assert_eq!(sel[1], 0x56);
        assert_eq!(sel[2], 0xcb);
        assert_eq!(sel[3], 0x59);
    }

    #[test]
    fn rlp_uint_zero_is_empty_string() {
        assert_eq!(rlp_uint(0), vec![0x80]);
    }

    #[test]
    fn fee_bump_applies_basis_points() {
        // +15% over 100 gwei.
        assert_eq!(bump_gas_price(100_000_000_000, 1500), 115_000_000_000);
        // Zero bump is identity.
        assert_eq!(bump_gas_price(42, 0), 42);
        // Bump on zero gas price stays zero.
        assert_eq!(bump_gas_price(0, 1500), 0);
    }

    #[test]
    fn fee_bump_saturates_instead_of_overflowing() {
        assert_eq!(bump_gas_price(u64::MAX, 10_000), u64::MAX);
    }

    #[test]
    fn rlp_uint_small_values() {
        // 0x01 encodes as 0x01 (single byte < 0x80)
        assert_eq!(rlp_uint(1), vec![0x01]);
        // 0x7f encodes as 0x7f
        assert_eq!(rlp_uint(0x7f), vec![0x7f]);
        // 0x80 encodes as 0x81 0x80
        assert_eq!(rlp_uint(0x80), vec![0x81, 0x80]);
    }
}
