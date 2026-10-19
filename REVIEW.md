# REVIEW.md

`runthrough` is a CLI that runs a repository's GitHub Actions workflows locally (in Docker or on the host), used to rehearse CI before pushing. Conventions live in [CLAUDE.md](CLAUDE.md); this file says what matters when reviewing a change here.

## What failure looks like here

- It lies: a workflow that would fail on GitHub passes locally (or the reverse), because an expression, `if:`, matrix, or `needs` result differs from GitHub's semantics, or the exit code does not reflect the job results.
- It leaks or damages: a secret is printed unmasked, or a step writes into the user's real repository or host when it should be confined to the job's scratch directory.

## Important

Flag as **Important** only when a change can:

- Diverge from GitHub's expression semantics in `src/expr.rs` (loose equality and coercion, case-insensitive comparison, `&&`/`||` returning operands, `.*` filters, status functions driven by `Ctx.failed`/`cancelled`).
- Change which jobs run or in what order: stage grouping and `-j` closure in `src/planner.rs`, matrix `include`/`exclude`, the trigger filter in `src/main.rs`, or `if:`/`continue-on-error`/`fail-fast` handling in `src/runner.rs`.
- Let a secret reach stdout: any output path in `src/runner.rs` that does not go through `Logger::line`, or a change to `mask` in `src/commands.rs` or to how `::add-mask::` values are registered.
- Mis-propagate step state: `GITHUB_OUTPUT`/`ENV`/`PATH`/`STATE` file handling in `JobRun::exec`, env precedence (runner → workflow → job → CLI `--env` → step), or post steps not running in reverse order.
- Write outside the job scratch root: `--bind` makes the workspace the user's real repository, `copy_repo` in `src/actions.rs` copies `.git`, and `src/backend.rs` relies on the scratch dir being mounted at the same absolute path in the container.
- Change CLI behaviour scripts depend on: flag names or defaults in `Cli` (`src/main.rs`), exit codes (`0` ok, `1` job failed, `2` internal error), or the per-job summary line (`✅ CI / test: success`).
- Break `make check` (`cargo check`, `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`, whose `tests/e2e.rs` asserts on stdout text).

Everything else is a Nit or is not posted.

## Always needs a human

A person reviews any change touching these, whatever the automated result:

- `src/commands.rs` — holds `mask`; a regression prints users' secrets to their terminal and CI logs.
- `src/backend.rs` — builds the `run` command line for `docker` and Apple's `container` (mounts, `--entrypoint`, options); a wrong mount exposes or modifies host paths.
- `src/actions.rs` — clones and executes third-party action code into the shared action cache and copies the user's repository; a mistake runs the wrong code or writes into the user's tree.
- `src/servers.rs` — listens on host ports (all interfaces for Apple containers) and writes uploaded caches and artifacts to disk; a path or bind mistake exposes or overwrites host files.
- `tests/fixtures/` — the e2e tests' only input; weakening a fixture silently removes coverage.

## Skip

Do not review (generated, vendored, or lock files):

- `Cargo.lock` — lockfile; review dependency changes in `Cargo.toml` instead.

## Nits

- At most 3 Nits per review, after every Important finding.
- Never flag what `rustfmt` or `clippy` (default lints) already enforce.

## Evidence

- Every finding cites `file:line` and the concrete input or sequence that fails (a workflow snippet, an expression, a CLI invocation). No guesses: an unproven worry is a question, not a finding.
- For semantics findings, state what GitHub Actions does for the same input and what `runthrough` does instead.
- A bug in lines this change did not touch is tagged **Pre-existing**, not charged to the author.
- Flag the class of bug once with every location, not one comment per line.

## Summary

Start with the count: `<n> Important, <n> Nit, <n> Pre-existing`. When nothing needs fixing, say "Nothing to fix" and stop.

## Learned

Rules promoted from repeated review comments, reverts and incidents. Each line names its source.

- None yet (2 commits, no reverts or fix history).
