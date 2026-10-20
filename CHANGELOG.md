# Changelog

Each release's section is published as its GitHub release notes (see [RUNBOOK.md](RUNBOOK.md#releasing)).

## 0.3.0

### Added

- `--install-hook` writes a git pre-push hook (`.git/hooks/pre-push`, or the one in `core.hooksPath`; never over an existing hook) that runs `runthrough --committed --quiet push`, so a failed job stops the push. `git push --no-verify` skips it once.
- `--committed` rehearses the HEAD commit instead of the working tree: the checkout and the workflows themselves, called ones included, come from HEAD, leaving uncommitted changes out.
- A run ends with one line for all of its workflows: jobs by result, the time it took and, when it was recorded, its dashboard URL. With `--watch`, each rerun also says which jobs changed result since the previous one.
- Without `-e`, `pull_request` and `pull_request_target` runs get a payload built from the repository: a PR from the current branch into the default branch (`--defaultbranch` picks another base), so `github.base_ref` and `github.head_ref` are set.
- A job using something a local run does not reproduce (`permissions`, environment protection, `concurrency` across runs, …) logs a note (`ℹ️`) when it starts; the dashboard shows the notes as annotations.
- The dashboard renders the jobs' step summaries (`GITHUB_STEP_SUMMARY`) on a run's summary page, and **Copy rehearsal note** copies the jobs and their results as Markdown for a pull request.
- A run that matches no workflow names the events the workflows do trigger on.

### Fixed

- The pre-push hook stops a push of any commit other than HEAD, which it does not rehearse.
- A built `pull_request` payload falls back to the local default branch for `base.sha` when `<remote>/<default>` is missing; `pull_request_target` no longer leaves `github.sha` at HEAD with path filters passing unchecked.

## 0.2.1

### Fixed

- Each image is pulled once per run instead of once per job; a failed pull of an image that exists locally is reported as a single `using the local <image>` line, and a missing one fails the job with the daemon's error.
- A container job fails with a hint to restart the engine when the engine does not answer its status probe within 30 seconds, instead of hanging silently (e.g. a hung Docker Desktop VM).

## 0.2.0

### Security

- The artifact API requires the job's `ACTIONS_RUNTIME_TOKEN`, now an HS256 JWT signed with a per-run key (or the caller's own token). Before, anyone who could reach the port could list, download, create and overwrite a run's artifacts.
- `run-name` masks secrets, and overlapping masks are merged, so a shorter secret contained in a longer one no longer leaves part of it visible.
- HTTP request bodies are read as they arrive, so an announced huge `Content-Length` can neither abort runthrough nor reserve memory up front.

### Fixed

- Job-level `success()` / `failure()` see every ancestor's result: a skipped or cancelled need no longer counts as failed, and failures several levels up are seen.
- `continue-on-error` is evaluated per matrix leg (`${{ matrix.experimental }}` applies), and a tolerated leg no longer cancels its siblings through `fail-fast`.
- A string literal holding `always(` or `failure()` in `if:` no longer drops the implicit `success() &&`.
- A matrix vector with no values fails the job, as GitHub does, instead of running it once without a matrix.
- Command properties decode `%25` last, and percent-decoding works on bytes, so a `%` before a multibyte character cannot panic.

## 0.1.0

Initial release.
