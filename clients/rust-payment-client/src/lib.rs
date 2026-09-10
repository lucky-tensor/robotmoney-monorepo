//! Canonical: Plan tracking issue #109 §4 — Phase 1 Rust client (library surface for integration tests)
//!
//! `rust-payment-client` library crate.
//!
//! Re-exports the modules used by the `rmpc` binary so integration tests
//! (and, later, embedders) can build against the same types. The binary
//! entry point lives in `src/main.rs`.

#![allow(dead_code)]

pub mod cli;
pub mod commands;
pub mod committee_identity;
pub mod config;
/// Canonicalization, digest derivation and Ed25519 verification for the swarm's
/// consensus recommendation receipt (issue #1247 / docs/architecture.md §4.9).
pub mod consensus_receipt;
pub mod errors;
pub mod fees;
pub mod gateway;
pub mod logging;
pub mod network_env;
pub mod nonce;
/// One owner for serialising a command's output document to stdout (issue #1285).
pub mod output;
pub mod policy;
pub mod read_output;
pub mod replay_cache;
pub mod rpc;
/// Dev-scout seam map for the off-chain scan-remediation phase (issue #994) —
/// documentation-only, no runtime code.
pub mod scan_remediation_seams;
/// Dev-scout seam map for the off-chain scan-remediation **residual** phase
/// (issue #1027) — the `get-agent` rolling-window gross seam for #1024.
/// Documentation-only, no runtime code; the bug it describes is still present
/// at HEAD.
pub mod scan_residual_seams;
pub mod signer;
pub mod tx;
