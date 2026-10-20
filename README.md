# runthrough

A run-through is the full rehearsal before opening night. `runthrough` runs your GitHub Actions workflows locally — an [act](https://github.com/nektos/act)-style runner written in Rust — so the show is rehearsed before you push.

## Install

```sh
brew tap simota/runthrough https://github.com/simota/runthrough   # macOS and Linux, prebuilt binary
brew trust --formula simota/runthrough/runthrough               # Homebrew 7+ loads third-party formulae only once trusted
brew install runthrough

cargo install --path .   # or build from a checkout
```

Requires `git`. Container jobs need `docker` (or Apple's `container` with `--engine container`); host jobs (`--host`) run with whatever shells and tools the host has (`bash`, `node`, `python`, `pwsh`, …).

## Usage

```sh
runthrough                      # run workflows triggered by `push`
runthrough pull_request -j test # one job (plus its `needs`) for another event
runthrough -l                   # list jobs by stage
runthrough -n                   # show the plan only
runthrough --host               # run on the host instead of Docker
runthrough --watch              # rerun whenever a file changes
runthrough --graph              # draw the job graph
runthrough --dashboard          # browse recorded runs at http://127.0.0.1:7766
runthrough --validate --strict  # check the workflows, then exit
runthrough --committed          # rehearse the HEAD commit, leaving uncommitted changes out
runthrough --install-hook       # add a git pre-push hook that runs the push workflows first
```

A run ends with one line for all of its workflows: jobs by result, the time it took and, when it was recorded, its dashboard URL. With `--watch`, each rerun also says which jobs changed result since the last one.

`--install-hook` writes `.git/hooks/pre-push` (or the hook in `core.hooksPath`), never over an existing hook. It runs `runthrough --committed --quiet push`, so the commit being pushed is rehearsed rather than the working tree (its checkout and the workflows themselves come from HEAD), and a failed job stops the push, as does pushing a commit other than HEAD, which the hook does not rehearse; `git push --no-verify` skips it once. Add flags such as `--host` to the hook's `runthrough` line.

Workflows are read from `.github/workflows` and, laid out the same way, `.runthrough/workflows` (for workflows meant only for local runs); `-W` reads a single file or directory instead.

Flags follow act's names (`--matrix`, `--reuse`, `--json`, `--input-file`, …); `runthrough --help` lists them and [docs/act-parity.md](docs/act-parity.md) tracks parity with act. Day-to-day operation and troubleshooting are in [RUNBOOK.md](RUNBOOK.md).

### Inputs and configuration

| Source | Flags | File (default) |
|---|---|---|
| Secrets | `-s KEY=VALUE` (a bare `KEY` reads the environment; an unset secret is asked for) | `.secrets` |
| Variables (`vars`) | `--var KEY=VALUE` | `.vars` |
| Step environment | `--env KEY=VALUE` | `.env` |
| `workflow_dispatch` inputs | `--input KEY=VALUE` | `.input` |
| Event payload | `-e event.json` | — |
| Default arguments | — | `$XDG_CONFIG_HOME/act/actrc`, `~/.actrc`, `./.actrc` |

Flags win over files. Arguments from `.actrc` files are read before the command line's. Without `-e`, a `merge_group` run gets a payload built from the repository (`head_sha`/`head_ref` from HEAD, `base_sha` where HEAD forked from the remote's default branch).

### Dashboard

Every run is recorded (with secrets masked) under `~/.cache/runthrough/runs`, and `runthrough --dashboard` serves a local web UI over them in the style of GitHub's Actions tab: the runs list (optionally grouped by repository or date), a run's job graph by stage with its matrix legs and annotations, and each job's steps with their logs, refreshed while a run is in progress. A run's summary page shows the jobs' step summaries (`GITHUB_STEP_SUMMARY`, as Markdown) and, as annotations, what the run did not reproduce locally; **Copy rehearsal note** copies the jobs and their results as Markdown for a pull request. A failed job's page has **Copy for LLM**, which copies the job's context, its steps and the last 200 lines of each failed step as Markdown to paste into an LLM. **Resources** lists the ports runthrough listens on and the Docker containers, networks and volumes it created, with the run each belongs to; ones whose run has ended are marked left behind, with a command to remove them. `--dashboard-addr` / `--dashboard-port` change where it listens; `--no-history` skips recording a run, and runs with `--insecure-secrets` are never recorded. The newest 100 runs are kept.

### Exit codes

`0` every job succeeded or was skipped, `1` a job failed, `2` runthrough itself failed (bad workflow, missing engine, …).

## Supported

- `on`, `run-name`, `env`, `defaults.run`, `concurrency`, `cache-mode`, `jobs.<id>.{needs, if, runs-on, environment, concurrency, cache-mode, container, services, env, outputs, strategy, timeout-minutes, continue-on-error}`
- `on.<event>` filters as on GitHub: `branches` / `tags` / `paths` (and `-ignore`), `types`, `workflow_run.workflows`, checked against the event payload (`-e`). An explicit `-j` or a single workflow file skips them; `paths` without changed files in the payload passes
- Steps with `background`, `wait`, `wait-all`, `cancel` and `parallel`
- Reusable workflows (`jobs.<id>.uses`, local or `owner/repo/path@ref`) with `with`, `secrets` / `secrets: inherit` and outputs; calls are checked against `on.workflow_call` (undeclared or missing required inputs and secrets), and cycles or more than 10 levels of nesting are refused
- Matrix expansion with `include` / `exclude`, `fail-fast`, `max-parallel`; jobs in a stage run in parallel
- Expressions: all operators, contexts (`github`, `env`, `vars`, `secrets`, `inputs`, `matrix`, `strategy`, `needs`, `steps`, `job`, `runner`), `.*` filters, and functions (`contains`, `startsWith`, `endsWith`, `format`, `join`, `toJSON`, `fromJSON`, `hashFiles`, `case`, `success`, `failure`, `always`, `cancelled`)
- `run` steps with `bash`, `sh`, `python`, `pwsh`, `node`, or custom `{0}` shells
- `uses`: local and remote (`owner/repo[/path]@ref`) **composite**, **node** and **docker** actions, including `pre`/`post`, and `docker://image` steps; `--local-repository owner/repo@ref=PATH` substitutes a local folder
- `actions/checkout` is emulated by copying the local repo (`--bind` mounts it instead, `--no-skip-checkout` runs the real action); other repositories are cloned
- A cache server for `actions/cache` (on by default; it also serves BuildKit's `type=gha` cache for `docker/build-push-action`) and, with `--artifact-server-path`, an artifact server for `upload-artifact` / `download-artifact` v3 and v4 (it only answers the run's own jobs, which carry its `ACTIONS_RUNTIME_TOKEN`)
- `GITHUB_OUTPUT`, `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY`, and `::set-output`, `::add-mask::`, `::group::`, `::error::`… commands, and problem matchers (`::add-matcher::`, `::remove-matcher::`); secrets are masked
- `workflow_dispatch` inputs are checked before a run: `required`, `choice` `options`, `boolean` / `number` values

Steps see `RUNTHROUGH=true` and `ACT=true` (also in the `env` context, so workflows that already special-case act keep working). `setup-*` actions download once: job containers share a tool cache at `/opt/hostedtoolcache` (kept under `~/.cache/runthrough/toolcache`; jobs sharing it run their `setup-*` steps one at a time, so two installs of the same version do not overwrite each other), and private images use `container.credentials` (or the `DOCKER_USERNAME` / `DOCKER_PASSWORD` secrets).

## Not supported

A job using one of these logs a note (`ℹ️`) when it starts. The cache service v2 API (`ACTIONS_CACHE_SERVICE_V2`). `permissions` and `jobs.<id>.snapshot` are validated but have no local effect; `environment` is reported (name and `url`) without environment secrets or protection rules. `concurrency` groups span one `runthrough` invocation, and a job cancelled by `cancel-in-progress` stops before its next step.

## Platforms

`ubuntu-*` labels default to `catthehacker/ubuntu:act-latest`. Override with `-P ubuntu-latest=node:20-bookworm-slim`, or `-P self-hosted=-self-hosted` to run a label on the host; on macOS, `self-hosted` and `macos-*` run on the host. Like a self-hosted runner, the host runs one such job at a time, since they share the machine (`~/.rustup`, the keychain); `--host` still runs every job in parallel.

- **Docker** (default). A remote Docker (`DOCKER_HOST=ssh://…` or `tcp://…`) works without bind mounts: job files live in volumes synced with `docker cp`.
- **Apple `container`** (`--engine container`; macOS 26+, Apple silicon, after `container system start`).
- **Host** (`--host`, or a label mapped to `-self-hosted`).

Job containers get no engine socket unless asked: `--daemon-socket-job <job>` mounts `/var/run/docker.sock` into that job only (for `docker compose` or `docker build` steps), `--container-daemon-socket <path>` into every job. A step that fails to reach the socket is told which flag to pass. A container job fails with a hint before its first pull when the engine is not running, or does not answer within 30 seconds (a hung Docker Desktop).

`--container-options` appends options to every job container's `run` (e.g. `--container-options '--dns 1.1.1.1'` when name resolution fails inside Apple containers).

## Development

```sh
make check CARGO_FLAGS=--offline   # typecheck + fmt --check + clippy -D warnings + tests
make ci CARGO_FLAGS=--offline      # run this repo's own CI workflow with runthrough --host
```

Tests that need Docker, Apple `container` or local ports are `#[ignore]`d; see [RUNBOOK.md](RUNBOOK.md#tests-and-act-parity) for running them and act's test suite.
