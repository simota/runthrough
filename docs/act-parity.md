# act parity checklist

Features [act](https://github.com/nektos/act) has that `runthrough` did not. Check an item off when it lands.

Sources (2026-10-16): act `cmd/root.go` on `master` for flags; act's
[not supported](https://nektosact.com/not_supported.html) page; `runthrough` at `bcba319`.
Items marked *(unverified)* come from general knowledge of act, not from those pages.

Tests: `cargo test` covers the host-runnable items; `cargo test -- --ignored` (outside a
sandbox) covers the servers, Docker and Apple `container`.

## Workflow features

- [x] Docker actions: `runs.using: docker` (Dockerfile or `docker://` image, `args`, `env`, `pre-entrypoint`, `post-entrypoint`) and `uses: docker://…` steps (`with.args`, `with.entrypoint`); act builds them (its `--rebuild` flag rebuilds local action Docker images). Run as sibling containers sharing the job's directories and network
- [x] Service containers (`jobs.<id>.services`) — on a per-job network with the service id as host name, `job.services.<id>.{id,network,ports}`, waits for `--health-cmd`. Apple `container` has neither network aliases nor health checks, so services are added to the job container's `/etc/hosts` and `--health-cmd` is polled with `container exec` *(act support unverified)*
- [x] Reusable workflows (`jobs.<id>.uses`) — local and `owner/repo/path@ref`, typed `with` inputs and defaults, `secrets` mapping or `inherit`, `on.workflow_call.outputs`, matrix callers *(act support unverified)*
- [x] `actions/checkout` of another repository — shallow clone of `repository` at `ref` into `path`, with `token` passed as a header
- [x] Artifact server (`--artifact-server-path`, `--artifact-server-addr`, `--artifact-server-port`) — the `@actions/artifact` v2 API used by `upload-artifact@v4` / `download-artifact@v4`; the v3 actions' API is not served
- [x] Cache server for `actions/cache` (`--no-cache-server`, `--cache-server-path`, `--cache-server-external-url`, `--cache-server-addr`, `--cache-server-port`) — the `@actions/cache` v1 API (`ACTIONS_CACHE_URL`), on by default and persisted under `~/.cache/runthrough/cache`

## Run control

- [x] `--watch` — rerun when files change
- [x] `--matrix` — run only matching matrix combinations
- [x] `--concurrent-jobs` — cap concurrent jobs (default: number of CPUs)
- [x] `--detect-event` — use the workflows' first event
- [x] `--reuse` — keep job containers and their directories under `~/.cache/runthrough/reuse` and reattach on the next run
- [x] `--rm` — accepted for compatibility; containers are always removed unless `--reuse` (act keeps failed ones without `--rm`)

## Output

- [x] `--graph` — draw the workflow graph
- [x] `--json` — JSON logs
- [x] `--quiet` — hide step output
- [x] `--log-prefix-job-id` — prefix logs with the job ID only
- [x] `--man-page`
- [x] `--bug-report`
- [x] `--list-options`

## Inputs and context

- [x] `--input-file` (default `.input`)
- [x] `--actor` (default `git config user.name`)
- [x] `--defaultbranch` (default from `refs/remotes/<remote>/HEAD`, else `main`)
- [x] `--remote-name`
- [x] `--insecure-secrets` — print secrets unmasked

## Workflow discovery

- [x] Recurse into subdirectories of `-W` (act's default; `--no-recurse` disables it)
- [x] Without `-W`, read `.runthrough/workflows` as well as `.github/workflows` (runthrough addition)

## Containers

- [x] `--privileged`
- [x] `--userns` — passed through; not exercised (Docker Desktop has no user namespaces)
- [x] `--container-cap-add`
- [x] `--container-cap-drop` — passed through like `--container-cap-add`
- [x] `--network`
- [x] `--container-daemon-socket` — mount a host socket at `/var/run/docker.sock` in job containers (opt-in; act mounts it by default)
- [x] `--daemon-socket-job ID` *(runthrough only)* — mount the socket only into the named jobs (repeatable; without `--container-daemon-socket`, `/var/run/docker.sock`), so `.actrc` can give it to a `docker compose` job without handing every job the engine and the host network
- [x] `--simulated-token-job ID` *(runthrough only)* — give the named jobs a placeholder `GITHUB_TOKEN` when none is set (repeatable), for actions that only require one to be present (`anchore/sbom-action` uploading to the artifact server); GitHub's API rejects it, even for public data, so it is not given to every job
- [x] `--rebuild` — rebuild Dockerfile action images (otherwise reused by a tag stable across runs)

## Checkout and action fetching

- [x] `--use-gitignore` — `--use-gitignore false` copies ignored files too
- [x] `--no-skip-checkout` — run the real `actions/checkout` (it needs `-s GITHUB_TOKEN=…`, as on act)
- [x] `--local-repository` — map `owner/repo@ref` to a local folder, for actions and reusable workflows
- [x] `--use-new-action-cache` — not applicable, no flag added: runthrough's cache already keeps one clone per `owner/repo@ref` and works offline (`--offline`, alias `--action-offline-mode`)

## GitHub Enterprise

- [x] `--github-instance` — `github.server_url` / `api_url` / `graphql_url` and the host actions are fetched from; fetching from a real GHES instance is not exercised
- [x] `--replace-ghe-action-with-github-com`
- [x] `--replace-ghe-action-token-with-github-com`

## Validation

- [x] `--validate` — load every workflow and check its job graph
- [x] `--strict` — reject keys outside the workflow, job and step schema

## Second pass: act's usage guide (2026-10-16)

Sources: nektosact.com [usage guide](https://nektosact.com/usage/index.html),
[runners](https://nektosact.com/usage/runners.html),
[custom container engine](https://nektosact.com/usage/custom_engine.html),
[GitHub Enterprise](https://nektosact.com/usage/github_enterprise.html),
[schema](https://nektosact.com/usage/schema.html); act `cmd/root.go` flag help; `runthrough` at `60955c4`.
"Confirmed" means the gap was reproduced with runthrough.

### Configuration and inputs

- [x] `.actrc` — default arguments (one per line; `--flag value` splits in two) from `$XDG_CONFIG_HOME/act/actrc`, `~/.actrc`, then `./.actrc`, before the command line
- [x] `--var NAME` / `--env NAME` without a value take it from the environment
- [x] `-s NAME` with `NAME` unset prompts on the terminal with echo off (`stty -echo`); without a terminal it fails with a clear error
- [x] An `"inputs"` object in the `-e` event payload sets inputs (precedence: defaults < payload < `--input`)
- [x] For `workflow_dispatch`, `github.event.inputs` holds the resolved inputs

### Expressions and context

- [x] `ACT` (and `RUNTHROUGH`) are in the `env` context, so `if: ${{ !env.ACT }}` skips a step

### Images and engines

- [x] `--pull` defaults to true and accepts `--pull=false`
- [x] `--action-offline-mode` keeps cached actions and present images (no pull) and still fetches missing ones; `--offline` stays strict
- [x] Version-specific default images: `ubuntu-<version>` → `catthehacker/ubuntu:act-<version>`; `ubuntu-latest` stays `act-latest`
- [x] A remote engine through `DOCKER_HOST` (`ssh://…`, `tcp://…`) — copy mode: the job root and action cache live in volumes synced with `docker cp`, Docker actions mount the same volumes; `--bind` and Docker actions in host jobs are refused. Exercised by forcing it on a local Docker (`RUNTHROUGH_REMOTE_ENGINE=1`); a real remote host is not

### Artifacts

- [x] `upload-artifact` / `download-artifact` v3 (`ACTIONS_RUNTIME_URL`), gzipped chunks included — round-tripped with the real v3 and v4 actions

### Validation

- [x] Schema validation on every run: required keys (`on`, `jobs`, `runs-on`/`uses`, `steps`, `run` xor `uses`) and value types; unknown keys only with `--strict`. Narrower than act's full JSON schema

## Third pass: act's source (2026-10-16)

Sources: act `master` — `pkg/model/github_context.go`, `pkg/model/workflow.go`,
`pkg/runner/run_context.go`, `pkg/runner/step.go`, `pkg/runner/command.go`,
`pkg/exprparser/functions.go`; `runthrough` at `60d9c96`. "Confirmed" means the gap was
reproduced with runthrough; "code" means it was read from runthrough's source.

Left out on purpose: act turns input names into `INPUT_*` by replacing every character
outside `A-Z0-9-` with `_`, but `@actions/core` (and runthrough) replace only spaces, so
copying act would break dotted inputs. `hashFiles` (SHA-256 over the per-file SHA-256
digests) is believed to match GitHub's runner rather than act; not verified against GitHub.

### Context and environment

- [x] `github.ref` / `sha` from the event payload: `pull_request*` → `refs/pull/<number>/merge`, `pull_request_target` → `refs/heads/<base ref>` and base SHA, `release` → `refs/tags/<tag_name>`, `deployment*` → `deployment.ref` / `.sha`, `push` / `create` / `workflow_dispatch` → `ref` and `after` (when `deleted` is false); `head_ref` / `base_ref` from `pull_request`
- [x] `ref_type` empty for `refs/pull/…` refs, `ref_name` from the derived ref
- [x] `GITHUB_ACTION_REF` for `uses:` steps; `github.action_ref` / `github.action_repository` inside remote composite actions
- [x] `ImageOS` from the job container's `/etc/os-release` when it is Ubuntu (so `catthehacker/ubuntu:act-latest` follows GitHub's `ubuntu-latest` as it moves to Ubuntu 26), else from the label (`ubuntu-22.04` → `ubuntu22`; `ubuntu-latest` → `ubuntu24`, where act says `ubuntu20`), `RUNNER_PERFLOG=/dev/null`, `RUNNER_TRACKING_ID`, `GITHUB_RETENTION_DAYS`, and `LANG=C.UTF-8` in job containers
- [x] `GITHUB_SERVER_URL` / `GITHUB_API_URL` / `GITHUB_GRAPHQL_URL` and `ACTIONS_RUNTIME_URL` / `ACTIONS_RUNTIME_TOKEN` from the caller's environment override the generated values; an explicit `--github-instance` wins over the URL variables, which are always set inside a GitHub Actions job
- [x] A tool cache that survives runs: job containers get `~/.cache/runthrough/toolcache/linux` at `/opt/hostedtoolcache` (a `runthrough-toolcache` volume with a remote engine); host jobs a per-OS directory beside it

### Workflow commands

- [x] `::set-env` and `::add-path` only when the job sets `ACTIONS_ALLOW_UNSECURE_COMMANDS=true` (as on GitHub); otherwise an error line points to `GITHUB_ENV` / `GITHUB_PATH`
- [x] Azure DevOps syntax `##[command key=value;]message`

### Containers

- [x] `container.credentials` / `services.<id>.credentials` (`username`, `password`), falling back to the `DOCKER_USERNAME` / `DOCKER_PASSWORD` secrets for the job container: `docker login` into a throwaway config, so `~/.docker` is untouched. Docker only (Apple's `registry login` writes the keychain). A failed login is exercised; a successful pull from a private registry is not
- [x] Not adopted: the engine socket stays opt-in (`--container-daemon-socket PATH`; `-` is accepted and mounts nothing), since mounting it by default hands every job control of the host's engine
- [x] Jobs without `container:` that have services, or that get the engine's socket (`--container-daemon-socket`, narrowed by `--daemon-socket-job`), share the host network (Docker), as act does for services and as GitHub's runner VM reaches services and the containers it starts itself (`docker compose up`) on `localhost`; other jobs keep the engine's default network. Apple's engine has no host network, so there services are reached by name
- [x] Jobs that publish the same host port (a service's fixed `ports: [5432:5432]`) run one at a time, as on GitHub each job has its own runner VM; here they share one engine, where the second `-p 5432:5432` fails with "port is already allocated"; a job given the engine's socket may publish anything (`docker compose up`), so it waits for, and holds off, every job publishing a fixed host port and every other job given the socket
- [x] Not adopted: containers are removed after every job so failed runs do not pile up; `--reuse` keeps them, `--keep` keeps the job directory

### Workflow model

- [x] `runs-on: {group, labels}` adds `group` to the labels (so `-P <group>=…` maps it)
- [x] A matrix `exclude` key that is not a matrix key is an error (as on GitHub)
- [x] Not adopted: `strategy.max-parallel` keeps GitHub's default (no limit); `--concurrent-jobs` caps jobs overall

## Fourth pass: act's planner, expressions, actions, caches (2026-10-16)

Sources: act `master` — `pkg/model/planner.go`, `pkg/exprparser/interpreter.go`,
`pkg/runner/action.go`, `pkg/runner/reusable_workflow.go`, `pkg/runner/step_action_remote.go`,
`pkg/artifactcache/handler.go`; `runthrough` at `f50934a`. "Confirmed" means reproduced with
runthrough; "code" means read from runthrough's source.

Already on par, so not listed: missing `needs` and cycles fail the
plan, `null == 0` / `'true' == true` coercion, `a[0]` / `a['x']` indexing, case-insensitive
context names, empty objects and arrays being truthy, the status-function default for `if`,
`owner/repo/path@ref` actions, `--replace-ghe-action-with-github-com`, and the skipped
`actions/checkout` emulation.

### Planning

- [x] Job IDs must match `^[A-Za-z_][A-Za-z0-9_-]*$` (as on GitHub)
- [x] A workflow that fails to load is reported on stderr and skipped while the others run; the run then exits 1 (2 when nothing loads)
- [x] Workflows triggered by the same event run side by side (each keeps its own stage order; `--concurrent-jobs` bounds jobs across them)

### Expressions

- [x] Objects and arrays never compare equal: values are copies here, so no two are the same instance (GitHub's rule); act raises an error instead

### Actions

- [x] A Docker action's `with.args` / `with.entrypoint` override `runs.args` / `runs.entrypoint` (as on GitHub)
- [x] A directory with a `Dockerfile` but no `action.yml` runs as a Docker action
- [x] Remote actions and reusable workflows are cloned with `secrets.GITHUB_TOKEN` when it is set (sent as a header, not stored), so private repositories work; not exercised against a private repository

### Cache server

- [x] Cache keys are case-insensitive (stored lowercased, as in act)
- [x] Old entries are collected when the server starts and after each upload: unused for 7 days, created over 30 days ago, superseded versions of a key, and uploads abandoned for 5 minutes
- [x] The cache API sits under a random per-run path (`ACTIONS_CACHE_URL=http://host:port/<token>/`); requests outside it get 404

## Fifth pass: act's step and job execution, artifact server (2026-10-16)

Sources: act `master` — `pkg/runner/step_run.go`, `pkg/runner/job_executor.go`,
`pkg/artifacts/server.go`, `pkg/artifacts/artifacts_v4.go`, `pkg/container/docker_run.go`;
`runthrough` at `dad8e91`. Already on par: pre → main → post
order with post steps in reverse, job result from step failures, the v3 artifact routes,
`.gz__` storage and missing authorization (runthrough's artifact server has none either).
act keeping failed jobs' containers is the "not adopted" item above.

- [x] PowerShell scripts get `$ErrorActionPreference = 'stop'` and end with `exit $LASTEXITCODE` (GitHub's wrapper) — reproduced before the fix: a failing native command left the step green
- [x] `cmd` shell (`cmd /D /E:ON /V:OFF /S /C "CALL "{0}""`, script prefixed with `@echo off`) and PowerShell as the default shell for host jobs on Windows (`pwsh`, else `powershell`); not exercised, as no Windows host was available
- [x] v4 upload/download URLs are signed (HMAC-SHA256 with a per-run key) and expire after 60 minutes; a bad or stale signature gets 401 — previously `sig=runthrough` was never checked
- [x] Images that run as a non-root user can write outputs and the workspace: copy mode creates and copies as root, then opens the files (`chmod a+rwX`); with bind mounts the job's own scratch directories are opened on the host (act chowns instead; the user's repository under `--bind` is never touched) — reproduced before the fix in copy mode
- [x] Exit code 127 is explained as "command not found"
- [x] The job container gets its job ID as a network alias on user-defined networks, so services can reach it
- [x] Not adopted: act allocates a TTY for `exec` when the terminal is one; runthrough parses step output line by line, and a TTY would merge stderr and add carriage returns

## Sixth pass: act's own test suite (2026-10-17)

`scripts/act_suite.py` runs act's runner integration tests from `pkg/runner/runner_test.go`
through runthrough with act's test settings (`--no-recurse`, `--actor nektos/act`,
`node:24-bookworm-slim`, each case's `event.json` and `config/config.yml`) and compares the
outcome with act's expected success or failure: the `TestRunEvent` table (jobs in
containers), the `TestRunEventHostEnvironment` tables (jobs on the host) and the
single-workflow tests (secrets, inputs, `pull_request`, `--matrix`, masking, post-step and
pull failures):

```sh
git clone --depth 1 https://github.com/nektos/act.git /path/to/act
python3 -I scripts/act_suite.py /path/to/act target/debug/runthrough "$TMPDIR/act-suite"
```

First run (`TestRunEvent` only): 59/87. Last run on macOS (all tables): 137/137. The
script swaps the image for three cases that failed for reasons outside runthrough:

- `docker-action-host-env`: its Dockerfile builds `FROM debian:bullseye-slim`, whose
  security archive returns 404 (a plain `docker build` fails the same way). The script runs
  a copy of the case under `<out dir>/patched` with the base image `debian:bookworm-slim`.
- Host `shells/pwsh` and `shells/sh`: macOS has no `pwsh` and its `/bin/sh` is bash; act
  runs the host tables on Linux only. Off Linux the script runs these two in
  `catthehacker/ubuntu:pwsh-latest` and `node:24-bookworm-slim` (dash as `/bin/sh`). Run on
  Linux (a `debian:12-slim` container with PowerShell, Node and a runthrough built in
  `rust:1-bookworm`), their host jobs pass as-is.

Differences the suite exposed, all fixed:

- [x] `defaults.run.shell` expressions (`${{ env.MY_SHELL }}`) were run unevaluated
- [x] A Docker action's `entrypoint: /bin/sh -c` is split: the first word is the entrypoint, the rest go before the args
- [x] `${{ insert }}` mapping keys merge an object into the mapping; in `strategy.matrix`, a list item that is one expression yielding a list is spliced in
- [x] State saved by `pre` reaches `main` and `post`; `pre-if` / `post-if` see the action's `inputs`
- [x] Remote actions' `pre` hooks run before the first step (local actions only exist after checkout, so theirs run in place)
- [x] An absolute `working-directory` such as `/root` is created inside the job container, not on the host
- [x] A Docker action's build context is its Dockerfile's directory (`image: ../Dockerfile`)
- [x] Running a workflow for `workflow_call` uses `on.workflow_call.inputs` defaults; `boolean` / `number` inputs are converted from strings
- [x] `job.container` is null for jobs without `container:`, so `job.container && 'sh' || 'bash'` picks bash
- [x] Step `env:` is evaluated with the job's env context intact, so `with:` still sees `GITHUB_ENV` additions
- [x] `--local-repository` directories are mounted into job containers; keys may start with `https://github.com/`
- [x] With a host-network job, services keep a bridge network and publish their ports to the host
- [x] A service container whose start fails is removed (it was left behind as `Created`)
- [x] Remote actions are fetched before the job container starts: cloned into the mounted cache afterwards, they were sometimes not visible inside the container on Docker Desktop (`Cannot find module 'which'`, 3 of 5 fresh runs; 0 of 8 after the fix)
- [x] A job whose runs-on labels map to no platform is skipped with a hint, as in act, instead of failing (found running the host tables on Linux, where `macos-latest` has no platform); a job whose matrix legs are all skipped reports `skipped`

## Seventh pass: GitHub's workflow syntax (2026-10-17)

Source: [Workflow syntax for GitHub Actions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).
Keys runthrough accepted but did not act on, or did not know:

- [x] `on.<event>` filters (`branches`, `branches-ignore`, `tags`, `tags-ignore`, `paths`, `paths-ignore`, `types`, `workflow_run.workflows`) decide whether a workflow runs, as on GitHub. **Differs from act**, which selects by event name only; `-j` or a single workflow file still bypasses them. Changed files come from the payload's `commits` or `before`..`after` (`pull_request`: `base.sha...head.sha`); without them `paths` passes. Without `-e`, a `push` payload's `before` is `<remote>/<branch>` when HEAD is ahead of it (what pushing now would send), so `paths` and actions such as `dorny/paths-filter` see the unpushed commits; an up-to-date, behind or new branch gets no `before`, and the checkout's `origin/<branch>` points at the commit being run, as after the push on GitHub
- [x] `steps[*].background`, `wait`, `wait-all`, `cancel`, `parallel`: background steps run on their own threads (at most 10 at once), their outputs, `GITHUB_ENV` / `GITHUB_PATH` additions and post steps apply when waited for, an implicit `wait-all` precedes the post steps, and `cancel` (or the job being cancelled, also while it waits for them) sends SIGTERM to the step's process group, then SIGKILL after 5s (a timeout kills the group; on Windows the process tree is ended with `taskkill`); on the host that group also ends when runthrough exits; `parallel` children's `pre` hooks run in the job's pre phase; rejected inside composite actions
- [x] `concurrency` (workflow and job): one holder per group (case-insensitive), the default queue replaces the pending run, `queue: max` keeps up to 100 in order, `cancel-in-progress` cancels the holder; a group an enclosing workflow or calling job already holds is cancelled as a deadlock (as GitHub does), and a called workflow's own `concurrency` applies (inside it `github.workflow` is the caller's, as on GitHub); `queue: max` with `cancel-in-progress` is a validation error. Groups span one runthrough invocation, and a cancelled holder stops before its next step
- [x] `cache-mode` (workflow and job: `read`, `write`, `write-only`, `none`) is enforced by the cache server: a denied restore is a miss, a denied save gets 403, which `@actions/cache` logs as "Failed to save" without failing the step; the upload, commit and download endpoints check the mode too, and only a pending reservation takes data
- [x] `jobs.<id>.environment`: the name is reported at job start and `url` after the steps (it may use `steps`); no deployments, environment secrets or protection rules
- [x] `services.<id>.command` and `entrypoint`
- [x] `run-name` is evaluated and printed
- [x] `permissions` values and `jobs.<id>.snapshot` are validated and otherwise ignored (no `GITHUB_TOKEN` is minted locally, no images are built)

## Eighth pass: GitHub features act lacks too (2026-10-17)

Sources: GitHub's [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax),
[expressions](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions),
[reusing workflows](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows)
and the runner's problem matcher format; `runthrough` at `cc0cf17`. All gaps were reproduced
with runthrough before the fix.

- [x] A reusable workflow that calls itself (directly or through others) failed with a
  thread panic; cycles and more than 10 levels (the caller plus nine) are now
  a job error naming the chain
- [x] Calls are checked against `on.workflow_call`: an input or secret the called workflow
  does not declare, or a required one not given (inputs with a `default` count as given), fails
  the calling job; `secrets: inherit` is not checked. `GITHUB_TOKEN` reaches the called workflow
  with a `secrets:` mapping too (it was dropped)
- [x] `case(pred1, val1, ..., default)`: predicates are tested for truthiness (the docs do not
  say whether non-boolean predicates are an error) and only the chosen value is evaluated
- [x] `workflow_dispatch` inputs are validated before the run: a
  missing `required` input, a `choice` outside its `options`, or a `boolean` / `number` value of
  the wrong type exits 2. Inputs a workflow does not declare are not refused, since `--input`
  applies to every workflow of the run. Both checks follow GitHub's documented input and
  secret declarations; the exact errors GitHub reports were not compared
- [x] Problem matchers: `::add-matcher::<file>` (a path relative to the workspace, read on the
  host, so in copy mode a file only present in the job container cannot be read) and
  `::remove-matcher owner=…::`, job-wide, replacing a matcher of the same owner; single- and
  multi-line patterns with `loop`, `severity` from a group or the matcher's default. Matching
  lines are logged as annotations (`❗` / `⚠️` / `ℹ️` with `file:line:col`). Patterns use the
  `regex` crate, so .NET-only syntax (look-around, back-references) is rejected when added;
  `fromPath` is parsed but not used to resolve `file`

