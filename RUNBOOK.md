# RUNBOOK

Operating `runthrough` day to day: where it keeps state, how to clean up, what to do when a run goes wrong, and how to check a change to runthrough itself. Features and flags are in [README.md](README.md).

## State on disk

Everything persistent lives under `$XDG_CACHE_HOME/runthrough` (default `~/.cache/runthrough`):

| Path | Contents | Safe to delete |
|---|---|---|
| `actions/` | Cloned remote actions (`--action-cache-dir` overrides) | Yes; refetched on the next online run, so not before an `--offline` run |
| `cache/` | `actions/cache` entries and `index.json` (`--cache-server-path` overrides) | Yes; caches start cold. Entries unused for 7 days, created over 30 days ago, or superseded by a newer save of the same key are removed automatically |
| `toolcache/` | `setup-*` downloads: `linux/` for job containers, `<os>-<arch>/` for host jobs | Yes; tools are downloaded again |
| `runs/` | Run history for `--dashboard`: `meta.json`, masked `log.jsonl`, `result.json` per run; the newest 100 are kept | Yes; the dashboard starts empty |
| `reuse/` | Per-job directories kept by `--reuse` | Yes, together with the `--reuse` containers |

Each job also gets a scratch directory (`workspace/`, `temp/`, `tool/`, `files/`) in the system temp dir, removed after the job unless `--keep` is set. In Docker jobs on macOS and Windows (Docker Desktop) the workspace is a volume (`runthrough-…-workspace`) unless `--bind` or the engine socket is given, since files written through Docker Desktop's file sharing can read back empty; the kept `workspace/` then holds only what was checked out, and files steps create there are not on the host.

## Cleanup

Containers, service containers and networks are removed at the end of each job. After a crash or `kill -9`, remove leftovers by name:

```sh
docker ps -a --filter name=runthrough- --format '{{.Names}}' | xargs -r docker rm -f
docker network ls --filter name=runthrough- -q | xargs -r docker network rm
docker volume ls --filter name=runthrough- -q | grep -vx runthrough-toolcache | xargs -r docker volume rm   # RUNNER_TEMP and remote-engine volumes
```

With `--engine container`, use `container list --all` and `container delete --force <name>`. Containers kept by `--reuse` are also named `runthrough-…`; the commands above remove them. The volume command keeps `runthrough-toolcache`, the tool cache shared by remote-engine jobs; remove it with `docker volume rm runthrough-toolcache` to download tools again.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Exit code `2`, `error: …` | runthrough itself failed (workflow did not validate, engine not found, …); the message says which. `runthrough --validate --strict` checks workflows alone |
| `the docker engine is not running: …` (or `the container engine …`) | Start Docker Desktop or the daemon (`container system start` for `--engine container`), or run on the host with `--host` |
| A step cannot reach `unix:///var/run/docker.sock` | Job containers get no engine socket by default. Pass `--daemon-socket-job <job>` (or `--container-daemon-socket <path>` for every job) |
| A service fails to start because its host port is taken | The error names the container publishing that port; stop it (often a leftover, see [Cleanup](#cleanup)) or change the service's `ports` |
| A workflow does not run for the event | Its `on.<event>` filters (`branches`, `paths`, `types`, …) rejected the payload. Pass a matching payload with `-e`, or run it directly with `-W <file>` or `-j <job>`, which skip the filters |
| A job is skipped with a platform hint | Its `runs-on` label maps to no image. Add `-P <label>=<image>` or `-P <label>=-self-hosted` |
| Image pull fails or is slow | Use local images with `--pull=false`; `--action-offline-mode` keeps present images and cached actions |
| No network | `--offline` uses only cached actions; run once online first to fill `actions/` |
| A secret appears in the output | Only values passed as secrets or with `::add-mask::` are masked; check the secret was passed with `-s` or `.secrets`, and that `--insecure-secrets` is not set |
| Name resolution fails in Apple containers | `--container-options '--dns 1.1.1.1'` |
| `docker cp` slowness with a remote engine | Expected: `DOCKER_HOST=ssh://`/`tcp://` copies job files instead of mounting them. `RUNTHROUGH_REMOTE_ENGINE=1` forces this mode locally for testing |
| Host job fails on `pwsh`/`sh` differences | Host jobs use the host's shells (macOS `/bin/sh` is bash). Run the job in a container with `-P` |
| `memory map must have a non-zero length`, or other files read back empty or missing in a Docker Desktop job | Docker Desktop's file sharing: paths bind-mounted from the host. The workspace and RUNNER_TEMP are volumes; with `--bind` the workspace is the repository itself, so set e.g. `--env CARGO_TARGET_DIR=/tmp/target` |
| Need to inspect a failed job's files | Rerun with `--keep` (scratch dir kept) and `-v` (debug messages, skipped steps, commands) |
| Something else | `runthrough --bug-report` prints the version, OS, engines, `git`/`node` versions and cache directory for an issue |

Exit codes: `0` every job succeeded or was skipped, `1` a job failed, `2` an internal error.

## Tests and act parity

Checks for a change to runthrough, run from the repository root:

```sh
make check CARGO_FLAGS=--offline   # typecheck + fmt --check + clippy -D warnings + unit and e2e tests
make ci CARGO_FLAGS=--offline      # dogfood: .github/workflows/build.yml through runthrough --host
```

`tests/e2e.rs` runs the built binary against `tests/fixtures/repo` with `--host` (needs `bash`, `node`, `git`). If it fails with `run runthrough: ... NotFound` after the repository was moved, `touch tests/e2e.rs` to rebuild the test binary.

Tests that need Docker, Apple `container` or local port binding are `#[ignore]`d. Run them outside a sandbox, with `debian:12-slim` and `pgvector/pgvector:pg16` pulled:

```sh
cargo test --offline -- --ignored
```

Parity with act is measured by running act's own runner tests through runthrough:

```sh
git clone --depth 1 https://github.com/nektos/act.git /path/to/act
cargo build
python3 -I scripts/act_suite.py /path/to/act target/debug/runthrough "$TMPDIR/act-suite" [case-filter]
```

It prints `PASS`/`FAIL` per case and a total; each case's log is in the output directory. The last result and the cases run with a swapped image are recorded in [docs/act-parity.md](docs/act-parity.md) (sixth pass); update it after a run.

## Releasing

1. Bump `version` in `Cargo.toml` (and `Cargo.lock` with `cargo check`), commit, and push.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`. The tag must match `Cargo.toml`.
3. `.github/workflows/release.yml` builds `runthrough` for `aarch64`/`x86_64` macOS and Linux, publishes `runthrough-<version>-<target>.tar.gz` with `.sha256` files as a GitHub release, and commits the formula generated by `scripts/homebrew_formula.py` to `Formula/runthrough.rb` in `simota/homebrew-tap`.

Updating the tap needs the `HOMEBREW_TAP_TOKEN` secret: a token with write access to `simota/homebrew-tap` (a fine-grained token with *Contents: read and write* on that repository). Without it the release is still published and the job warns. To write the formula by hand, download the `.sha256` files into a directory and run `python3 -I scripts/homebrew_formula.py <version> <dir>`.
