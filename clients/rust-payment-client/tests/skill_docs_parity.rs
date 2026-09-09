//! Canonical: Plan tracking issue #109 §10 (skill packaging).
//!
//! Automated parity checks for the `plugins/robotmoney-cli/` skill package.
//!
//! Replaces two manual review items on issue #52:
//!   (A) Manual review against `rmpc --help`.
//!   (B) Manual review by loading the skill in a harness.
//!
//! This test binary does NOT require an RPC fixture, devnet, or docker.
//! It builds `rmpc`, runs its `--help` output, and cross-references the
//! shipped skill markdown to fail loudly if drift exists between the
//! documentation and the actual CLI surface.
//!
//! Two test groups:
//!
//!   * `cli_help_parity::*`  — every command and flag mentioned in the
//!     skill docs must exist in `rmpc --help` (or in the relevant
//!     subcommand's `--help`); every subcommand surfaced by `rmpc --help`
//!     must be documented somewhere in the skill package.
//!
//!   * `skill_package_structure::*` — `plugin.json` parses, declares the
//!     skill, the SKILL.md path resolves, the YAML-style frontmatter is
//!     present and well-formed, and every `references/*.md` file linked
//!     from SKILL.md exists on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::OnceLock;

use assert_cmd::Command;

/// Repo root, located by walking up from `CARGO_MANIFEST_DIR` until we
/// find a `plugins/` directory next to `clients/`.
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur: &Path = &manifest;
    loop {
        if cur.join("plugins").is_dir() && cur.join("clients").is_dir() {
            return cur.to_path_buf();
        }
        cur = cur.parent().expect(
            "walked past filesystem root without finding repo root \
             (expected sibling `plugins/` and `clients/` directories)",
        );
    }
}

fn skill_dir() -> PathBuf {
    repo_root().join("plugins/robotmoney-cli/skills/robotmoney-cli")
}

fn skill_md_path() -> PathBuf {
    skill_dir().join("SKILL.md")
}

fn references_dir() -> PathBuf {
    skill_dir().join("references")
}

fn plugin_json_path() -> PathBuf {
    repo_root().join("plugins/robotmoney-cli/plugin.json")
}

/// Read every shipped doc file in the skill package. The first read
/// builds the cache; subsequent calls reuse it.
fn all_doc_text() -> &'static String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut joined = String::new();
        for path in doc_files() {
            joined.push_str(
                &fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
            );
            joined.push('\n');
        }
        joined
    })
}

fn doc_files() -> Vec<PathBuf> {
    let mut v = vec![skill_md_path()];
    let refs = references_dir();
    let mut entries: Vec<PathBuf> = fs::read_dir(&refs)
        .unwrap_or_else(|e| panic!("read references dir {}: {e}", refs.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    entries.sort();
    v.extend(entries);
    v
}

/// Run `rmpc --help` (or `rmpc <args> --help`) and return stdout. Uses
/// `assert_cmd` so the binary is built once via cargo's normal test
/// pipeline.
fn rmpc_help(args: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("rmpc").expect("rmpc binary built");
    cmd.args(args).arg("--help");
    let out = cmd.output().expect("spawn rmpc --help");
    assert!(
        out.status.success(),
        "`rmpc {} --help` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).expect("rmpc --help stdout is utf-8")
}

/// Subcommand names listed under the `Commands:` section of `rmpc <args>
/// --help`. Empty for a leaf command, which has no such section.
///
/// Clap prints subcommands under a `Commands:` heading, indented two spaces,
/// name then description.
fn subcommands_under(args: &[&str]) -> BTreeSet<String> {
    let help = rmpc_help(args);
    let mut in_commands = false;
    let mut out: BTreeSet<String> = BTreeSet::new();
    for line in help.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if trimmed.is_empty() {
            // First blank line after `Commands:` ends the block.
            if !out.is_empty() {
                break;
            }
            continue;
        }
        // Skip non-indented section headers (e.g. `Options:`).
        if !line.starts_with("  ") {
            break;
        }
        let name = line.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        // `help` is a built-in clap command we don't surface in the skill docs.
        if name == "help" {
            continue;
        }
        out.insert(name.to_string());
    }
    out
}

/// Top-level subcommand list from `rmpc --help`.
fn rmpc_subcommands() -> BTreeSet<String> {
    static CACHE: OnceLock<BTreeSet<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let help = rmpc_help(&[]);
            let out = subcommands_under(&[]);
            assert!(
                !out.is_empty(),
                "could not parse subcommand list from `rmpc --help`:\n{help}"
            );
            out
        })
        .clone()
        .into_iter()
        .collect()
}

/// Long-flag list (`--something`) for a subcommand PATH from its `--help`
/// output. Caches per path.
///
/// The path may be nested (`["committee", "vote-submit"]`): a flag declared on
/// a leaf is invisible in the parent's help, so a union built only from
/// top-level `--help` output would reject every leaf flag the skill docs name.
fn rmpc_subcommand_flags(path: &[&str]) -> BTreeSet<String> {
    static CACHE: OnceLock<std::sync::Mutex<BTreeMap<String, BTreeSet<String>>>> = OnceLock::new();
    let mu = CACHE.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
    let key = path.join(" ");
    {
        let guard = mu.lock().unwrap();
        if let Some(v) = guard.get(&key) {
            return v.clone();
        }
    }
    let help = rmpc_help(path);
    let mut flags = BTreeSet::new();
    for tok in help.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
        if let Some(rest) = tok.strip_prefix("--") {
            // Skip empty / numeric-prefix junk.
            if rest.is_empty() {
                continue;
            }
            // Filter to flags that look like real long flags: must start
            // with a letter and contain only [a-z0-9-].
            let valid = rest
                .chars()
                .next()
                .map(|c| c.is_ascii_lowercase())
                .unwrap_or(false)
                && rest
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if valid {
                flags.insert(format!("--{rest}"));
            }
        }
    }
    let mut guard = mu.lock().unwrap();
    guard.insert(key, flags.clone());
    flags
}

/// Extract every `rmpc <subcommand>` token mentioned in inline-code
/// spans (single backticks) or fenced code blocks. Mentions in prose
/// like "rmpc is the Robot Money..." are intentionally ignored — only
/// formatted command examples count as documentation of the surface.
fn documented_subcommands() -> BTreeSet<String> {
    let text = all_doc_text();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for span in code_spans(text) {
        let words: Vec<&str> = span.split_whitespace().collect();
        for win in words.windows(2) {
            if win[0] != "rmpc" {
                continue;
            }
            let next = win[1].trim_matches(|c: char| !(c.is_ascii_lowercase() || c == '-'));
            if next.is_empty() || next.starts_with('-') {
                continue;
            }
            if !next.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                continue;
            }
            out.insert(next.to_string());
        }
    }
    out
}

/// Yield every backtick-delimited inline code span and every fenced
/// code block body. Both forms are how the skill docs render command
/// examples — anything outside them is considered prose.
fn code_spans(text: &str) -> Vec<String> {
    let mut spans: Vec<String> = Vec::new();
    // Fenced code blocks (```...```).
    let mut in_fence = false;
    let mut fence_buf = String::new();
    let mut prose_for_inline = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                spans.push(std::mem::take(&mut fence_buf));
                in_fence = false;
            } else {
                in_fence = true;
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push('\n');
        } else {
            prose_for_inline.push_str(line);
            prose_for_inline.push('\n');
        }
    }
    // Inline `...` spans inside non-fenced text.
    let mut chars = prose_for_inline.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '`' {
            continue;
        }
        let mut buf = String::new();
        for ch in chars.by_ref() {
            if ch == '`' {
                break;
            }
            buf.push(ch);
        }
        if !buf.is_empty() {
            spans.push(buf);
        }
    }
    spans
}

/// Extract every `--flag` token mentioned in code spans of the skill
/// docs. The token must look like a long flag: `--[a-z][a-z0-9-]*`.
fn documented_flags() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let joined: String = code_spans(all_doc_text()).join(" ");
    for tok in joined.split(|c: char| c.is_whitespace() || ",()[]{}*\"".contains(c)) {
        if let Some(rest) = tok.strip_prefix("--") {
            let trimmed = rest.trim_end_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-'));
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed
                .chars()
                .next()
                .map(|c| c.is_ascii_lowercase())
                .unwrap_or(false)
            {
                continue;
            }
            if !trimmed
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                continue;
            }
            out.insert(format!("--{trimmed}"));
        }
    }
    out
}

// ---------------------------------------------------------------------
// (A) `rmpc --help` parity checks.
// ---------------------------------------------------------------------

#[test]
fn every_documented_subcommand_exists_in_rmpc_help() {
    let actual = rmpc_subcommands();
    let documented = documented_subcommands();
    let missing: Vec<&String> = documented.iter().filter(|s| !actual.contains(*s)).collect();
    assert!(
        missing.is_empty(),
        "skill docs reference subcommands that `rmpc --help` does not list: {:?}\n\
         actual rmpc subcommands: {:?}",
        missing,
        actual,
    );
}

#[test]
fn every_rmpc_subcommand_is_documented_in_skill_package() {
    let actual = rmpc_subcommands();
    let documented = documented_subcommands();
    let undocumented: Vec<&String> = actual.iter().filter(|s| !documented.contains(*s)).collect();
    assert!(
        undocumented.is_empty(),
        "`rmpc --help` lists subcommands that the skill docs do not mention: {:?}\n\
         documented subcommands: {:?}",
        undocumented,
        documented,
    );
}

#[test]
fn every_documented_flag_exists_on_some_rmpc_subcommand() {
    // Build the union of flags exposed by `rmpc --help` (for
    // global flags like `--help`/`--version`) and every subcommand
    // `--help`. A documented flag is allowed if it appears anywhere in
    // that union.
    let mut union: BTreeSet<String> = BTreeSet::new();
    // Top-level flags: parse `rmpc --help` directly (no subcommand).
    {
        let help = rmpc_help(&[]);
        for tok in help.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
            if let Some(rest) = tok.strip_prefix("--") {
                if rest.is_empty() {
                    continue;
                }
                let valid = rest
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_lowercase())
                    .unwrap_or(false)
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                if valid {
                    union.insert(format!("--{rest}"));
                }
            }
        }
    }
    // Walk the whole command tree, not just its first level. `rmpc committee`
    // and `rmpc receipt` are subcommand GROUPS: their real flags
    // (`--weight-bps`, `--receipt-url`, ...) live on the leaves and never
    // appear in the parent's help, so a union built from top-level help alone
    // would reject every leaf flag the skill docs publish.
    for sub in rmpc_subcommands() {
        union.extend(rmpc_subcommand_flags(&[&sub]));
        for leaf in subcommands_under(&[&sub]) {
            union.extend(rmpc_subcommand_flags(&[&sub, &leaf]));
        }
    }

    // Flags clap always supplies. They may not appear in every
    // subcommand's help body but are always accepted.
    union.insert("--help".into());
    union.insert("--version".into());

    // Documented `--flag` tokens, minus a small set of non-flag tokens
    // that legitimately appear in prose (like the `--idempotency-key`
    // *example* form vs the actual flag — included anyway because the
    // flag exists). No exclusions today; all documented flags must
    // resolve.
    let documented = documented_flags();

    let missing: Vec<&String> = documented.iter().filter(|f| !union.contains(*f)).collect();
    assert!(
        missing.is_empty(),
        "skill docs reference flags that no `rmpc` subcommand exposes: {:?}\n\
         union of all subcommand flags: {:?}",
        missing,
        union,
    );
}

// ---------------------------------------------------------------------
// (B) Skill-package structural validation.
// ---------------------------------------------------------------------

#[test]
fn plugin_json_is_valid_and_resolves_skill_paths() {
    let path = plugin_json_path();
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&raw).expect("plugin.json must be valid JSON");

    // Required top-level fields per the harness loader contract.
    for key in ["name", "version", "description", "skills"] {
        assert!(
            v.get(key).is_some(),
            "plugin.json missing required field `{key}`"
        );
    }

    let name = v["name"].as_str().expect("plugin.json `name` is a string");
    assert_eq!(
        name, "robotmoney-cli",
        "plugin.json `name` must match the package directory"
    );

    let _version = v["version"]
        .as_str()
        .expect("plugin.json `version` is a string");

    let skills = v["skills"]
        .as_array()
        .expect("plugin.json `skills` is an array");
    assert!(
        !skills.is_empty(),
        "plugin.json must declare at least one skill"
    );

    let plugin_dir = path.parent().unwrap();
    for s in skills {
        let id = s.as_str().expect("each `skills` entry is a string");
        let skill_md = plugin_dir.join("skills").join(id).join("SKILL.md");
        assert!(
            skill_md.is_file(),
            "plugin.json declares skill `{id}` but {} does not exist",
            skill_md.display()
        );
    }
}

#[test]
fn skill_md_has_frontmatter_with_required_fields() {
    let raw = fs::read_to_string(skill_md_path()).expect("read SKILL.md");
    assert!(
        raw.starts_with("---\n"),
        "SKILL.md must start with `---` YAML frontmatter delimiter"
    );
    let after = &raw[4..];
    let end = after
        .find("\n---")
        .expect("SKILL.md frontmatter must terminate with `---`");
    let frontmatter = &after[..end];
    // Cheap structural checks — we don't pull in a YAML parser for two
    // fields. The frontmatter must declare `name:` and `description:`.
    assert!(
        frontmatter
            .lines()
            .any(|l| l.trim_start().starts_with("name:")),
        "SKILL.md frontmatter missing `name:` field:\n{frontmatter}"
    );
    assert!(
        frontmatter
            .lines()
            .any(|l| l.trim_start().starts_with("description:")),
        "SKILL.md frontmatter missing `description:` field:\n{frontmatter}"
    );

    // The declared `name:` must match the directory name.
    let name_line = frontmatter
        .lines()
        .find(|l| l.trim_start().starts_with("name:"))
        .unwrap();
    let name = name_line
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .trim_matches('"');
    assert_eq!(
        name, "robotmoney-cli",
        "SKILL.md frontmatter `name` must match the skill directory"
    );
}

#[test]
fn skill_md_links_to_existing_reference_files() {
    let raw = fs::read_to_string(skill_md_path()).expect("read SKILL.md");
    let dir = references_dir();

    // Find every markdown link of the form `references/<file>.md` (with
    // optional leading backtick). Backticks are stripped before parsing.
    let cleaned: String = raw.replace('`', "");
    let mut linked: BTreeSet<String> = BTreeSet::new();
    for tok in cleaned.split(|c: char| c.is_whitespace() || "()[]<>".contains(c)) {
        let stripped = tok.trim_end_matches(|c: char| ".,;:!?\"'".contains(c));
        if let Some(rest) = stripped.strip_prefix("references/") {
            if rest.ends_with(".md") {
                linked.insert(rest.to_string());
            }
        }
    }
    assert!(
        !linked.is_empty(),
        "SKILL.md must link to at least one references/*.md file"
    );
    for f in &linked {
        let p = dir.join(f);
        assert!(
            p.is_file(),
            "SKILL.md links to references/{f} but {} does not exist",
            p.display()
        );
    }

    // Also assert each on-disk reference doc is actually mentioned —
    // otherwise we have orphans the agent runtime won't discover.
    for path in fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
        let name = path.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        assert!(
            linked.contains(&name),
            "references/{name} exists on disk but SKILL.md does not link to it"
        );
    }
}

#[test]
fn cargo_metadata_resolves_repo_root() {
    // Sanity: the resolver above must agree with `git rev-parse`.
    let out = StdCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("git rev-parse --show-toplevel");
    assert!(
        out.status.success(),
        "git rev-parse --show-toplevel failed (not a git repo?)"
    );
    let git_root = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    let resolved = repo_root();
    assert_eq!(
        fs::canonicalize(&git_root).unwrap(),
        fs::canonicalize(&resolved).unwrap(),
        "repo_root() walker disagreed with `git rev-parse --show-toplevel`"
    );
}
