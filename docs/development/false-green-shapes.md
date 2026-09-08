# False-green CI shapes

A "false green" is a CI result that reports success while proving nothing
about the thing it claims to guard. This catalogue exists so that claim is
checkable after the fact: each shape below has a **stable name** a review can
cite in `checked_shapes`, the **mechanism** by which a green result is
possible, at least one **instance** already found in this repository (with a
workflow path and the issue that recorded it), and the **check** that
detects it.

The catalogue is recovered from issues already filed in this repository, not
invented. It is enforced by
[`check_false_green_catalogue.py`](../../.github/scripts/check_false_green_catalogue.py),
run in suite 13 (`doc-validators`) on every PR with no `paths` filter — see
[ci-suites.md §13](./ci-suites.md#13-cross-cutting-doc-checks). That check
asserts this file exists, that every section below carries all four required
subsections, and that every workflow path and issue number cited here
resolves.

When a review names a shape from this catalogue, it should cite the `###
Name` value, not the section heading — the heading is free text for a
reader; the name is the stable identifier.

---

## A named evidence script does not exist

### Name
`missing-evidence-script`

### Mechanism
A doc records a script path as the executed evidence that closes some
guarantee or gap. The script named is never written. Because it does not
exist, no workflow can invoke it, and nothing about the guarantee's claim is
ever exercised — the doc simply asserts closure by naming a file, and the
file is fictional.

### Instance
`docs/development/headless-opencode-tests.md` recorded
`.github/scripts/tests/negative_control_drop_plugin_flag.sh` as the negative
control closing guarantee **G11** ("Suite-11b did not exercise the in-repo
plugin manifest or prove on-chain deposit delta"). That file was never
created under `.github/scripts/tests/` (issue #1235). The closure claim was
withdrawn rather than backed after the fact; the withdrawn reference is kept
in the doc, in past tense, as the historical record of the defect, and is
allow-listed in the detecting check below so that record does not itself
trip it.

### Detecting check
`.github/scripts/check_evidence_scripts.py` invariant (A): every
`.github/scripts/...` path named in `docs/**` or `.github/workflows/**` must
exist on disk, modulo a commented allow-list for deliberate historical
exceptions. Wired into `.github/workflows/suite-13-doc-checks.yml`
(`doc-validators` job), which carries no `paths` filter.

---

## A script exists but no workflow invokes it

### Name
`uninvoked-evidence-script`

### Mechanism
A script sits under the evidence/negative-control home
(`.github/scripts/tests/`) and is named in a doc as backing some claim, but
no `.github/workflows/*.yml` step actually runs it. The script can neither
pass nor fail — it contributes zero executed coverage while reading, on
paper, like a control that exists.

### Instance
`.github/scripts/tests/negative_control_keystore_generate_flag.sh` existed
and was named at `docs/development/headless-opencode-tests.md:242`, but no
workflow step ran it (issue #1235). It now runs in
`.github/workflows/suite-11b-opencode-headless.yml`'s `asserter-tests` job on
every trigger including `pull_request`.

### Detecting check
`.github/scripts/check_evidence_scripts.py` invariant (B): every file
directly under `.github/scripts/tests/` must be referenced on a non-comment
line in at least one `.github/workflows/*.yml` file. Wired into
`.github/workflows/suite-13-doc-checks.yml` (`doc-validators` job), which
carries no `paths` filter.

---

## A guard's git plumbing is silently starved of input

### Name
`starved-guard-input`

### Mechanism
An assertion executes and reports PASS, but the git command feeding it
failed first and had its error swallowed (a `2>/dev/null`, an `|| echo
<fallback>`). The pipeline that consumes that empty output cannot observe a
violation, because there is no input to observe one in — so the check always
prints PASS regardless of whether the thing it claims to check actually
happened. The executed-assertion floor does not catch this shape: the
assertion runs and counts, it just cannot see anything.

### Instance
`plugins/robotmoney-swarm/tests/check-restricted-paths.sh`'s restricted-path
coupling guard, run from `.github/workflows/suite-17-swarm-plugin.yml`. The
job's `actions/checkout@v4` had no `fetch-depth`, so the runner got a
depth-1 clone: `git merge-base HEAD origin/dev` failed, `HEAD~1` did not
exist either, and the swallowed failures left `grep -q` with nothing to
match — the guard printed PASS on every PR regardless of what changed,
including PR #1196, which touched exactly the restricted paths the guard
exists to catch (issue #1232).

### Detecting check
`check-restricted-paths.sh --self-test`'s "unresolvable base" case asserts
that a base commit that cannot be resolved makes the guard exit non-zero
(loud failure) rather than PASS. In CI, `suite-17-swarm-plugin.yml` now uses
`fetch-depth: 0` and resolves the base from
`github.event.pull_request.base.sha` rather than guessing.

---

## A quoted or non-ASCII path evades an anchored regex

### Name
`quoted-path-evasion`

### Mechanism
`git diff --name-only` (and similar plumbing) C-quotes any path containing a
non-ASCII byte, a double quote, a backslash, a tab, or a newline, under
git's default `core.quotePath=true`. A guard that matches emitted paths with
a regex anchored at start-of-line (`^(contracts/|...)`) does not match a
line that now begins with `"`, so a restricted path named with a quoting
byte evades detection entirely, and the guard reports clear.

### Instance
`plugins/robotmoney-swarm/tests/check-restricted-paths.sh:65`'s
`RESTRICTED_RE` is anchored `^(contracts/|crates/|clients/rust-payment-client/|services/)`
and does not unquote its input. A branch touching
`plugins/robotmoney-swarm/**` together with `contracts/café.sol` was
reproduced exiting 0 (no coupling violation reported); renaming the same
file to `contracts/plain.sol` made the identical branch exit 1 (issue
#1252). The repo owner's decision on that issue was to close it **not
planned**: zero tracked paths in the repo currently contain such a byte
(`git ls-files | LC_ALL=C grep -cP '[^\x20-\x7e]'` → 0), so the hole is
accepted as a residual, unreachable-by-accident risk rather than fixed, and
`.github/workflows/suite-17-swarm-plugin.yml` ships the guard unchanged.

### Detecting check
None. This is the one shape in this catalogue with no automated detector —
recorded here, rather than left to be rediscovered, precisely because its
absence was a deliberate decision (issue #1252) and not an oversight. A
reviewer citing this shape by name should treat it as an accepted residual,
not an open defect, unless a tracked path with a quoting byte is
introduced — `git ls-files | LC_ALL=C grep -cP '[^\x20-\x7e]'` is the
one-line check that would need to become a real CI job if that ever happens.

---

## A guard-extraction harness can be neutered by the file it guards

### Name
`selftest-env-splice`

### Mechanism
A harness extracts a workflow step's body and executes it under a
sandboxed environment (`env -i` plus a controlled `PATH`/`HOME`) so the
step's logic can be proven correct at PR time even though the workflow
itself only fires on a different trigger. If the harness splices the step's
own declared `env:` values in *after* its sandbox assignments, `env`'s
last-wins semantics let the workflow under test override the harness's own
containment variables — so an edit to the very file being tested can make
the harness's assertions vacuous, or worse, execute attacker-chosen code
before the extracted body runs (e.g. via `BASH_ENV`), while every assertion
still reports green.

### Instance
`scripts/release/install-rmpc-selftest.sh`'s `env -i PATH=... "${STEP_ENV[@]}"`
invocations placed the extracted step's own `env:` block last. Reverting
`release-rmpc.yml`'s packaging step to a bare `sha256sum` and adding a
step-level `env: PATH: <real PATH>` made the selftest report 30/30 passed
while still printing that it had proven the macOS no-`sha256sum` case
(issue #1242, found reviewing `.github/workflows/release-rmpc.yml` via PR
#1222).

### Detecting check
`install-rmpc-selftest.sh`'s fixtures (`step-env-path`, `step-env-bash-env`,
`duplicate-step`, `env-moved-up`) run a hostile copy of `release-rmpc.yml`
through the same extractor and assert each bypass is refused as an
extractor error. Invoked via `plugins/robotmoney-swarm/tests/run-tests.sh`
in `.github/workflows/suite-17-swarm-plugin.yml` on every PR.

---

## A canonical doc contradicts the invariant its guard depends on

### Name
`doc-contradicts-guard-invariant`

### Mechanism
A workflow's correctness depends on an invariant that is not visible in the
workflow file itself — for example, "this job must have no `paths` filter,
because the check inside it must run on PRs a path filter would exclude."
If the canonical doc that describes the workflow states the invariant
*wrong* (claiming a `paths` filter the workflow doesn't have, or omitting a
step that depends on the no-filter property), a future change that
"reconciles" the workflow to match its own canonical doc reintroduces the
exact defect the invariant exists to prevent — silently, because the check
becomes *not run* rather than *failed*.

### Instance
`.github/workflows/suite-06-rmpc-unit.yml` declares
`docs/development/ci-suites.md` §6 canonical and rests on "this suite has no
paths filter... so the guard cannot be bypassed." But §6 still said `Trigger
paths: clients/rust-payment-client/**` and never mentioned the
`plugin_skill_command_examples` step that needs the no-filter property
(issue #1231) — the same false-green shape issues #1199 and #1203 were filed
about, this time one document-edit away from reintroducing itself.

### Detecting check
None automated for the general class — this instance was caught by review,
not CI, and the fix was a doc correction (`ci-suites.md` §6 now states the
invariant and its reason, and lists the `plugin_skill_command_examples`
step). Recorded here so a reviewer checking `.github/workflows/*.yml`
against `docs/development/ci-suites.md` treats a trigger/doc mismatch as
this shape rather than a cosmetic drift.

---

## Trigger-scoped code that never runs at PR time

### Name
`trigger-scoped-dead-code`

### Mechanism
A job's trigger condition (a tag push, a schedule, `workflow_dispatch`)
means its code path is never exercised by the CI run a pull request
actually gets. A reviewer reading the diff of that file at PR time is
reading code that has not run in this pipeline at all — review can catch a
typo, but it cannot catch "this only works on `ubuntu-latest`" the way an
execution would, because there is no execution to catch it with. This is
not itself a bug in the workflow; it becomes a false-green risk when nothing
else exercises the trigger-scoped logic at PR time, and reviewers infer
coverage that isn't there.

### Instance
`.github/workflows/release-rmpc.yml` triggers only on `push: tags: [rmpc-v*]`
and `workflow_dispatch`, and its own header says so explicitly: "This
workflow only fires on a tag, so without [a] selftest none of it would be
exercised anywhere." Found during the `review-security` pass on PR #1222,
which named the specific suspicion — that the packaging/checksum logic
never runs on a PR — and asked the reviewer to judge whether the existing
cover was adequate (loop session `2026-08-26-013021`, finding S-10).

### Detecting check
`scripts/release/install-rmpc-selftest.sh` extracts and executes the
`Package tar.gz` and `Assert every archive ships its checksum` step bodies
from `release-rmpc.yml` directly — not a grep of the step text, an actual
run — so the trigger-scoped logic is exercised on every PR despite the
workflow itself never firing on one. Invoked via
`plugins/robotmoney-swarm/tests/run-tests.sh` in
`.github/workflows/suite-17-swarm-plugin.yml`. A reviewer citing this shape
should ask, for any tag/dispatch-only job: what PR-time proxy actually runs
this code, and does `--self-test` prove that proxy itself can fail.

---

## A tool absent on a matrix runner turns a downstream guard into a skip, not a fail

### Name
`matrix-tool-absent-step-skipped`

### Mechanism
A downstream job depends (`needs:`) on every leg of a matrix build
completing. If one leg's tool availability assumption is wrong for its
runner (a GNU-coreutils command absent on `macos-latest`, say), that leg
fails — and because the downstream job merely `needs:` the matrix with no
`if: always()`, GitHub Actions marks the downstream job **skipped**, not
failed. A skipped required check is not the same signal as a failed one:
where a failure is unambiguous and loud, a skip can read as "not
applicable" rather than "the release pipeline is broken," and the guarded
step's absence is easy to miss in a status list dominated by other green
checks.

### Instance
`.github/workflows/release-rmpc.yml`'s `Package tar.gz` step ran a bare
`sha256sum`, which is GNU coreutils and does not exist on the `macos-latest`
half of the `build` matrix (macOS ships `shasum` instead). The step's own
comment records the mechanism exactly: "A bare `sha256sum` here exits 127
under the default `bash -e` shell, failing both macOS builds and skipping
`publish` (which `needs: build`) — so a tag would publish nothing at all,
not even the Linux archives." Rated `high` by the `review-security` pass on
PR #1222, on a different axis than the prior suspicion it was asked to
evaluate (loop session `2026-08-26-013021`, finding S-10; tracked in issue
#1242's motivation).

### Detecting check
The packaging step now branches explicitly — `command -v sha256sum`, else
`command -v shasum`, else `echo "::error::..."; exit 1` — so a runner
missing both tools fails loudly instead of silently 127-ing into a matrix
skip. `scripts/release/install-rmpc-selftest.sh` extracts this exact step
with `sha256sum` removed from `PATH` and asserts it still produces a
checksum (via the `shasum` branch) rather than silently producing nothing,
run via `plugins/robotmoney-swarm/tests/run-tests.sh` in
`.github/workflows/suite-17-swarm-plugin.yml` on every PR.

---

## An `Option`-returning fixture turns a missing dependency into a passing test

### Name
`optional-fixture-early-return`

### Mechanism
A test fixture that needs an external resource (a database, a devnet RPC, model
weights) is written as a fallible constructor returning `Option`/`nil`/`None`,
and every caller unwraps it with an early `return`:

```rust
let Some(fx) = try_pg_fixture().await else { return; };
```

When the resource is absent the constructor hands back its "unavailable"
sentinel, the test body never executes, and the function returns normally — which
is precisely how a test harness encodes **success**. The job prints
`test result: ok. N passed`, the executed-test guard
(`.github/scripts/cargo_test_require_executed.sh`) sees a healthy non-zero count,
and nothing anywhere distinguishes "asserted and held" from "asserted nothing."
The suite is greenest exactly when it verified least. The two mechanisms are
complementary and this shape falls in the gap between them: the executed-count
guard counts *passed tests*, not *executed assertions*.

An environment-gated escalation (panic when `CI` is set, skip otherwise) narrows
the window but does not close it, because the fixture still returns an `Option`
and the `else { return; }` guard at the call site stays legal and copy-pasteable.
The structural fix is to delete the sentinel: a fixture that returns the resource
itself, and panics naming the missing dependency, gives a caller no `None` to
match on.

### Instance
`services/watchdog/tests/common/mod.rs::try_pg_fixture` returned
`Option<PgFixture>` and all ten of its callers in `tests/cursor_and_volume.rs` and
`tests/threshold_breach.rs` wrote the guard above (issue #1377). Measured on the
pre-fix tree with `docker` removed from `PATH`, both binaries reported
`test result: ok` — 3 of 4 and 7 of 7 tests "passing" in 0.00s having booted no
container. PR #1376 had added `assert!(!config.action.has_pauser_key(), ...)` inside
`pause_rpc_timeout_does_not_starve_alert`, one of the ten, so that assertion would
have silently not run. It did not bite: run 34148606346 provisioned real containers
(8.4s container boot, ~1.4-2.6s per test), and the same property was independently
proven by a dependency-free unit test — this was a latent shape, not an outage.
`clients/explorer-api/tests/canonical_schema.rs` carried the same one-call-site
version, also fixed under #1377. The `services/explorer-indexer/` fixture keeps the
`Option` return with an environment-gated escalation added by issue #1283; the
residual structural risk and `fork_indexer.rs`'s ungated variant are tracked in
issue #1383.

### Detecting check
The fixtures no longer return `Option` — `try_pg_fixture() -> Option<PgFixture>`
became `pg_fixture() -> PgFixture` in `services/watchdog/tests/common/mod.rs`, and
`try_pool() -> Option<PgPool>` became `pg_pool() -> PgPool` in
`clients/explorer-api/tests/canonical_schema.rs`. Both panic with a
`REQUIRED DEPENDENCY UNAVAILABLE` prefix naming Docker, so the `else { return; }`
guard is not merely discouraged but **uncompilable** — a future caller cannot
reintroduce the shape at these call sites without first re-adding the sentinel.
That is the whole of the automated protection, and its reach is narrower than a
one-line summary would suggest:

- The panic reds `.github/workflows/suite-20-watchdog.yml`'s `watchdog-integration`
  job, which carries no `if:` guard and so runs on every PR, drafts included. Only
  that job's `cursor_and_volume` step wraps the run in
  `.github/scripts/cargo_test_require_executed.sh`; its `alert_webhook` and
  `threshold_breach` steps are plain `cargo test`. For those two the fixture panic
  is the *only* thing standing between a Docker-less runner and a green job.
- It also reds `.github/workflows/suite-08-explorer-indexer.yml`'s
  `explorer-api-committee-regime` job, which does run all four of its targets
  through the executed-count guard and runs `docker version` up front — but which
  is gated `if: … github.event.pull_request.draft == false`. It does not run on
  draft PRs, so that red arrives at ready-for-review, before merge, rather than on
  every push.

**Nothing detects a _new_ instance of this shape.** The type signature protects the
call sites that exist; no check would catch a future author introducing another
`Option`-returning fixture elsewhere in the tree, and the surviving
`services/explorer-indexer/` fixture is guarded only by an environment variable
(issue #1383). Re-verify by hand: run a built test binary with `docker` absent from
`PATH` and confirm a non-zero exit, as recorded on PR #1381.
