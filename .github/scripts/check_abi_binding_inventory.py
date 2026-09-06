#!/usr/bin/env python3
"""Fail CI when an ABI binding file is un-gated and untracked (issue #1346).

Canonical: docs/architecture.md §5.2 — Agent Permissions Gateway (ABI artifact
pipeline); docs/development/ci-suites.md §16.

WHY THIS EXISTS
---------------
Foundry's `out/` is the single canonical ABI source, and
`.github/scripts/generate_abi_bindings.sh` re-derives the client bindings from
it so `suite-16-abi-drift.yml` can diff them. But the script also *lists*, in a
header comment, the files it deliberately does not regenerate — hand-maintained
excerpts and copies with entries the Rust client needs at compile time. Those
files are outside every gate.

Before this check, that list said "known schema drift, not yet CI-gated /
tracked separately" and named no issue. Nothing tracked them, nothing measured
them, and nothing noticed when two more files (`InvestmentCommitteePolicy.json`,
`TimelockController.json`) were added to the same directory and never appeared
in the list at all. One of the un-gated files,
`clients/rust-payment-client/abi/VaultRegistry.json`, still declares a nine-field
`getVault` return that `contracts/VaultRegistry.sol` stopped returning — the
same defect issue #1348 fixed on the dapp side. It sat there because a comment
saying "tracked separately" is not tracking.

An exemption with no expiry is a permanent exemption. This check makes the
inventory a machine-checked contract instead of a comment:

  1. every ABI file on disk is classified — drift-gated, or un-gated;
  2. every un-gated file cites a tracking issue number;
  3. (with --verify-issues-open) that issue is actually OPEN, so closing the
     tracking issue without doing the work turns CI red rather than silently
     restoring the "tracked separately" fiction;
  4. every file claimed as drift-gated is genuinely named in the workflow's
     `git diff --exit-code` list, so a file cannot be gated in the comment and
     ungated in reality.

USAGE
-----
    python3 .github/scripts/check_abi_binding_inventory.py
    python3 .github/scripts/check_abi_binding_inventory.py --verify-issues-open
    python3 .github/scripts/check_abi_binding_inventory.py --self-test

`--verify-issues-open` shells out to `gh` and needs `GH_TOKEN`/`GITHUB_TOKEN`.
CI always passes it. It fails loudly when `gh` is missing or unauthenticated —
it never downgrades to the offline check, because a resource check that
silently no-ops is the same class of defect this script exists to catch.

`--self-test` drives the checker's own failure paths against synthetic trees.
A guard only ever observed passing is a guard nobody has checked.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# Header markers in generate_abi_bindings.sh delimiting the two inventories.
GATED_HEADER = "# FULLY-GENERATED OUTPUTS (CI drift-gated)"
UNGATED_HEADER = "# UN-GATED HAND-MAINTAINED FILES (each MUST cite an OPEN tracking issue)"

GENERATOR = ".github/scripts/generate_abi_bindings.sh"
WORKFLOW = ".github/workflows/suite-16-abi-drift.yml"

# Where ABI binding files live. Every file matching one of these globs must be
# classified by the generator header. Adding a binding directory here is how a
# new client surface joins the inventory.
DISCOVERY_GLOBS = (
    "clients/rust-payment-client/abi/*.json",
    "clients/dapp/src/lib/abi.generated.ts",
)

# A header line naming a file: `#   <path> — <prose>`. The path is the first
# whitespace-delimited token and must look like a repo-relative binding file.
_ENTRY_RE = re.compile(r"^#\s{2,}(\S+\.(?:json|ts))\b(.*)$")
_ISSUE_RE = re.compile(r"\(tracking issue #(\d+)\)")


def _parse_block(lines: list[str], header: str) -> list[tuple[str, str]]:
    """Return `(path, trailing prose)` for each entry under `header`.

    The block ends at the first line that is not a comment, or at a bare `#`
    (blank comment line) that is followed by another block header.
    """
    try:
        start = next(i for i, line in enumerate(lines) if line.rstrip() == header)
    except StopIteration:
        raise SystemExit(
            f"ERROR: {GENERATOR} has no `{header}` line.\n"
            "       The inventory check parses that header. If you renamed it, update\n"
            "       .github/scripts/check_abi_binding_inventory.py to match."
        )

    entries: list[tuple[str, str]] = []
    for line in lines[start + 1 :]:
        if not line.startswith("#"):
            break
        if line.rstrip() == "#":
            # A blank comment line ends the block only once entries were seen;
            # this tolerates prose paragraphs between the header and the list.
            if entries:
                break
            continue
        match = _ENTRY_RE.match(line)
        if match:
            entries.append((match.group(1), match.group(2)))
    return entries


def _workflow_gated_paths(workflow_text: str) -> set[str]:
    """Paths named in suite-16's `git diff --exit-code` drift check.

    Only the backslash-continued argument list of the `git diff --exit-code`
    command counts. Scanning the whole file would let a path listed under the
    workflow's `paths:` trigger filter masquerade as a gated file — a trigger
    decides *whether the job runs*, not whether anything diffs the file.
    """
    paths: set[str] = set()
    lines = workflow_text.splitlines()
    for index, line in enumerate(lines):
        if "git diff --exit-code" not in line:
            continue
        cursor = index
        while cursor < len(lines) - 1 and lines[cursor].rstrip().endswith("\\"):
            cursor += 1
            match = re.search(r"(\S+\.(?:json|ts))(?![\w.])", lines[cursor])
            if match:
                paths.add(match.group(1))
    return paths


def _gh_issue_state(number: str) -> str:
    """OPEN / CLOSED for a GitHub issue, via `gh`. Raises loudly on failure."""
    if shutil.which("gh") is None:
        raise SystemExit(
            "ERROR: --verify-issues-open needs the `gh` CLI and it is not on PATH.\n"
            "       This check must not fall back to the offline inventory: a\n"
            "       tracking issue that was closed is exactly what it is looking for."
        )
    # In Actions, resolve against GITHUB_REPOSITORY rather than letting `gh`
    # infer the repo from the checkout's remote.
    cmd = ["gh", "issue", "view", number, "--json", "state,title"]
    repo = os.environ.get("GITHUB_REPOSITORY")
    if repo:
        cmd += ["--repo", repo]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise SystemExit(
            f"ERROR: `gh issue view {number}` failed (exit {proc.returncode}).\n"
            f"       stderr: {proc.stderr.strip()}\n"
            "       Set GH_TOKEN/GITHUB_TOKEN, or drop --verify-issues-open to run\n"
            "       the offline inventory check only."
        )
    return json.loads(proc.stdout)["state"].upper()


def check(root: Path, verify_issues_open: bool = False) -> list[str]:
    """Return a list of problems; empty means the inventory is sound."""
    problems: list[str] = []

    generator = root / GENERATOR
    if not generator.is_file():
        return [f"{GENERATOR} not found under {root}"]
    lines = generator.read_text().splitlines()

    gated = _parse_block(lines, GATED_HEADER)
    ungated = _parse_block(lines, UNGATED_HEADER)
    gated_paths = [p for p, _ in gated]
    ungated_paths = [p for p, _ in ungated]

    # (1) No file may be claimed both gated and un-gated.
    for path in sorted(set(gated_paths) & set(ungated_paths)):
        problems.append(
            f"{path} is listed as BOTH drift-gated and un-gated in {GENERATOR}. "
            "It is one or the other."
        )

    # (2) Every listed file must exist.
    for path in gated_paths + ungated_paths:
        if not (root / path).exists():
            problems.append(
                f"{GENERATOR} lists {path}, which does not exist. Delete the entry, "
                "or restore the file."
            )

    # (3) Every un-gated file must cite a tracking issue.
    cited: dict[str, str] = {}
    for path, prose in ungated:
        match = _ISSUE_RE.search(prose)
        if match is None:
            problems.append(
                f"{path} is recorded as un-gated in {GENERATOR} with no tracking "
                "issue. Append `(tracking issue #NNNN)` naming an OPEN issue that "
                "will bring it under the gate — an exemption with no expiry is a "
                "permanent exemption (issue #1346)."
            )
        else:
            cited[path] = match.group(1)

    # (4) Every ABI file on disk must be classified by one block or the other.
    classified = set(gated_paths) | set(ungated_paths)
    for pattern in DISCOVERY_GLOBS:
        for found in sorted(root.glob(pattern)):
            rel = found.relative_to(root).as_posix()
            if rel not in classified:
                problems.append(
                    f"{rel} exists but appears in neither inventory block of "
                    f"{GENERATOR}. Add it to the drift-gated list (and to "
                    f"{WORKFLOW}'s diff), or to the un-gated list with a tracking "
                    "issue."
                )

    # (5) Everything claimed as drift-gated must really be diffed by the workflow.
    workflow = root / WORKFLOW
    if not workflow.is_file():
        problems.append(f"{WORKFLOW} not found under {root}")
    else:
        diffed = _workflow_gated_paths(workflow.read_text())
        for path in gated_paths:
            if path not in diffed:
                problems.append(
                    f"{path} is listed as drift-gated in {GENERATOR} but is not "
                    f"named in {WORKFLOW}, so nothing diffs it. Add it to the "
                    "`git diff --exit-code` list."
                )

    # (6) Optionally: the cited issues must still be open.
    if verify_issues_open:
        for path, number in sorted(cited.items()):
            state = _gh_issue_state(number)
            if state != "OPEN":
                problems.append(
                    f"{path} cites tracking issue #{number}, which is {state}. "
                    "Either the un-gating work is done (regenerate the file and "
                    "move it under the gate) or it is not (reopen the issue, or "
                    "cite the successor). A closed tracker is not tracking."
                )

    return problems


# ---------------------------------------------------------------------------
# Self-test — drive the checker's own failure paths.
# ---------------------------------------------------------------------------

_GEN_TEMPLATE = """#!/usr/bin/env bash
# generate_abi_bindings.sh
#
{gated_header}
{gated_entries}
#
{ungated_header}
{ungated_entries}
#
# end
set -euo pipefail
"""

_WF_TEMPLATE = """name: abi-drift-gate
on:
  pull_request:
    paths:
{trigger_lines}
jobs:
  abi-drift:
    steps:
      - run: |
          git diff --exit-code \\
{diff_lines}
"""


def _seed(tmp: Path, gated: list[str], ungated: list[str], on_disk: list[str],
          diffed: list[str], triggers: list[str] | None = None) -> Path:
    root = tmp
    (root / ".github/scripts").mkdir(parents=True, exist_ok=True)
    (root / ".github/workflows").mkdir(parents=True, exist_ok=True)
    for rel in on_disk:
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("[]\n")
    (root / GENERATOR).write_text(
        _GEN_TEMPLATE.format(
            gated_header=GATED_HEADER,
            gated_entries="\n".join(f"#   {e}" for e in gated) or "#",
            ungated_header=UNGATED_HEADER,
            ungated_entries="\n".join(f"#   {e}" for e in ungated) or "#",
        )
    )
    (root / WORKFLOW).write_text(
        _WF_TEMPLATE.format(
            trigger_lines="\n".join(f'      - "{t}"' for t in (triggers or [])) or "      - \"**\"",
            diff_lines="\n".join(f"            {d} \\" for d in diffed),
        )
    )
    return root


def _self_test() -> int:
    abi = "clients/rust-payment-client/abi"
    cases: list[tuple[str, dict, bool, str]] = [
        (
            "compliant inventory",
            dict(
                gated=[f"{abi}/Erc20.json"],
                ungated=[f"{abi}/MockVault.json — excerpt (tracking issue #1362)"],
                on_disk=[f"{abi}/Erc20.json", f"{abi}/MockVault.json"],
                diffed=[f"{abi}/Erc20.json"],
            ),
            True,
            "",
        ),
        (
            "un-gated entry with no tracking issue",
            dict(
                gated=[f"{abi}/Erc20.json"],
                ungated=[f"{abi}/MockVault.json — known schema drift, tracked separately"],
                on_disk=[f"{abi}/Erc20.json", f"{abi}/MockVault.json"],
                diffed=[f"{abi}/Erc20.json"],
            ),
            False,
            "no tracking issue",
        ),
        (
            "ABI file on disk in neither block",
            dict(
                gated=[f"{abi}/Erc20.json"],
                ungated=[f"{abi}/MockVault.json — excerpt (tracking issue #1362)"],
                on_disk=[
                    f"{abi}/Erc20.json",
                    f"{abi}/MockVault.json",
                    f"{abi}/TimelockController.json",
                ],
                diffed=[f"{abi}/Erc20.json"],
            ),
            False,
            "appears in neither inventory block",
        ),
        (
            "listed file that does not exist",
            dict(
                gated=[f"{abi}/Erc20.json"],
                ungated=[f"{abi}/Ghost.json — excerpt (tracking issue #1362)"],
                on_disk=[f"{abi}/Erc20.json"],
                diffed=[f"{abi}/Erc20.json"],
            ),
            False,
            "which does not exist",
        ),
        (
            "gated file the workflow never diffs",
            dict(
                gated=[f"{abi}/Erc20.json", f"{abi}/RobotMoneyGateway.json"],
                ungated=[f"{abi}/MockVault.json — excerpt (tracking issue #1362)"],
                on_disk=[
                    f"{abi}/Erc20.json",
                    f"{abi}/RobotMoneyGateway.json",
                    f"{abi}/MockVault.json",
                ],
                diffed=[f"{abi}/Erc20.json"],
            ),
            False,
            "nothing diffs it",
        ),
        (
            "gated file that only appears in the workflow's paths: trigger",
            dict(
                gated=[f"{abi}/Erc20.json", f"{abi}/RobotMoneyGateway.json"],
                ungated=[f"{abi}/MockVault.json — excerpt (tracking issue #1362)"],
                on_disk=[
                    f"{abi}/Erc20.json",
                    f"{abi}/RobotMoneyGateway.json",
                    f"{abi}/MockVault.json",
                ],
                diffed=[f"{abi}/Erc20.json"],
                triggers=[f"{abi}/RobotMoneyGateway.json"],
            ),
            False,
            "nothing diffs it",
        ),
        (
            "file claimed both gated and un-gated",
            dict(
                gated=[f"{abi}/Erc20.json", f"{abi}/MockVault.json"],
                ungated=[f"{abi}/MockVault.json — excerpt (tracking issue #1362)"],
                on_disk=[f"{abi}/Erc20.json", f"{abi}/MockVault.json"],
                diffed=[f"{abi}/Erc20.json", f"{abi}/MockVault.json"],
            ),
            False,
            "BOTH drift-gated and un-gated",
        ),
    ]

    failures = 0
    for name, seed_kwargs, expect_pass, expected_text in cases:
        with tempfile.TemporaryDirectory() as tmp:
            root = _seed(Path(tmp), **seed_kwargs)
            problems = check(root)
        passed = not problems
        if passed != expect_pass:
            failures += 1
            print(
                f"SELF-TEST FAIL: {name!r} expected "
                f"{'no problems' if expect_pass else 'a problem'}, got: {problems}"
            )
            continue
        if not expect_pass and not any(expected_text in p for p in problems):
            failures += 1
            print(
                f"SELF-TEST FAIL: {name!r} fired, but no message mentioned "
                f"{expected_text!r}: {problems}"
            )
            continue
        print(f"SELF-TEST OK: {name}")

    if failures:
        print(f"\n{failures} self-test case(s) failed — the guard is not trustworthy.")
        return 1
    print(f"\nAll {len(cases)} self-test cases passed.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--verify-issues-open",
        action="store_true",
        help="also resolve each cited tracking issue via `gh` and require it be OPEN",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the guard against synthetic trees and exit",
    )
    parser.add_argument(
        "--root",
        default=None,
        help="repository root (default: two levels above this script)",
    )
    args = parser.parse_args()

    if args.self_test:
        return _self_test()

    root = Path(args.root) if args.root else Path(__file__).resolve().parents[2]
    problems = check(root, verify_issues_open=args.verify_issues_open)

    if problems:
        print("ERROR: ABI binding inventory is not sound (issue #1346).\n")
        for problem in problems:
            print(f"  - {problem}")
        print(
            f"\nThe inventory lives in {GENERATOR}'s header. Every ABI binding file "
            "is\neither regenerated from a Foundry artifact and diffed by "
            f"{WORKFLOW},\nor recorded as un-gated with an OPEN tracking issue."
        )
        return 1

    print("ABI binding inventory is sound: every binding is gated or tracked.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
