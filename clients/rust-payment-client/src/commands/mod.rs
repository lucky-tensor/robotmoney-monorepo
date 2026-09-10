//! Canonical: Plan tracking issue #109 §4.8 — CLI surface
//!
//! `rmpc` subcommand implementations.
//!
//! Each module exposes a `run(...)` function that returns the process exit
//! code. JSON output goes on stdout; logs/warnings go on stderr.

pub mod committee;
pub mod committee_identity;
pub mod deposit;
pub mod get_agent;
pub mod get_allowance;
pub mod get_balance;
pub mod get_deposit;
pub mod get_gateway;
pub mod get_governance;
pub mod get_roles;
pub mod get_router;
pub mod get_timelock;
pub mod get_tx;
pub mod get_vault;
pub mod get_vaults;
pub mod governance_draft;
pub mod propose;
pub mod receipt;
pub mod self_check;
pub mod status;
pub mod vote;
pub mod withdraw;
pub mod withdraw_router;

#[cfg(test)]
mod boundary_tests {
    //! Lint: a command module must not import from a sibling command
    //! module (issue #1285).
    //!
    //! Before this rule, `withdraw_router` imported the policy rule
    //! `withdraw_vault_preflight` from `withdraw`, `withdraw` and
    //! `withdraw_router` imported `MAX_DEADLINE_SKEW_SECS` from `deposit`,
    //! and three write commands imported the refusal payload type
    //! `ChecksOutput` from the `self_check` diagnostic command. Each of
    //! those is a shared concern that belongs to a layer — `policy`,
    //! `write_path`, `output` — not to whichever command happened to need
    //! it first.
    //!
    //! The rule is enforced by reading the source tree rather than by a
    //! visibility rule, because `pub(crate)` items are legitimately
    //! reachable from anywhere in the crate.

    use std::path::PathBuf;

    fn commands_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands")
    }

    #[test]
    fn no_command_module_imports_from_a_sibling_command_module() {
        let dir = commands_dir();
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

        let mut checked = 0usize;
        let mut violations: Vec<String> = Vec::new();
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "mod.rs" {
                continue;
            }
            checked += 1;
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            for (n, line) in src.lines().enumerate() {
                let trimmed = line.trim_start();
                // Doc comments and comments are prose, not imports — the
                // module docs deliberately name where moved items went.
                if trimmed.starts_with("//") {
                    continue;
                }
                // `super` inside a command module is `crate::commands`, so
                // a column-0 `use super::` is a sibling import. Indented
                // ones live inside `mod tests` and refer to the module
                // itself, which is fine.
                if line.contains("crate::commands::") || line.starts_with("use super::") {
                    violations.push(format!("{name}:{}: {}", n + 1, line.trim()));
                }
            }
        }

        assert!(
            checked >= 20,
            "expected to scan the whole commands/ directory, only saw {checked} modules — \
             the scan has drifted from the source layout",
        );
        assert!(
            violations.is_empty(),
            "command modules must not import from sibling command modules; move the shared item \
             into a layer module (policy / write_path / output / errors) instead:\n  {}",
            violations.join("\n  "),
        );
    }
}
