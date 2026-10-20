//! Runs the fixture repository's workflows on the host (requires `bash` and `node`).
//! Tests marked `#[ignore]` need local port binding or a container engine; run them
//! with `cargo test -- --ignored` outside a sandbox.

use std::process::Command;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/repo");

/// The binary, kept out of the user's run history (`history_records_masked_runs` checks it).
fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_runthrough"));
    c.arg("--no-history");
    c
}

/// A temporary git repository for workflows written by the test.
struct TempRepo(tempfile::TempDir);

impl TempRepo {
    fn new() -> TempRepo {
        let repo = TempRepo(tempfile::tempdir().unwrap());
        assert!(Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        std::fs::create_dir_all(repo.path().join(".github/workflows")).unwrap();
        repo
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }

    /// Write `.github/workflows/<name>`.
    fn workflow(&self, name: &str, yml: &str) {
        std::fs::write(self.path().join(".github/workflows").join(name), yml).unwrap();
    }

    /// `bin()` run in the repository with `args`: its stdout.
    fn run(&self, args: &[&str]) -> String {
        let out = bin()
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn runthrough_full(args: &[&str]) -> (i32, String, String) {
    let out = bin()
        .args(["-C", FIXTURE, "--host", "--no-cache-server"])
        .args(args)
        .output()
        .expect("run runthrough");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn runthrough(args: &[&str]) -> (i32, String) {
    let (code, out, _) = runthrough_full(args);
    (code, out)
}

/// `-l` lists every job by stage; `-n` prints the stages of the triggered workflows.
#[test]
fn list_and_dryrun_print_stages() {
    let (code, out) = runthrough(&["-l"]);
    assert_eq!(code, 0, "{out}");
    let header = out.lines().next().unwrap_or_default();
    assert!(header.starts_with("STAGE  JOB ID"), "{out}");
    assert!(header.ends_with("EVENTS"), "{out}");
    let down = out
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some("down"))
        .expect(&out);
    assert_eq!(
        down.split_whitespace().collect::<Vec<_>>(),
        ["1", "down", "down", "artifacts.yml", "page_build"],
        "{out}"
    );

    let (code, out) = runthrough(&["push", "-n"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "/.github/workflows/demo.yml)\n  stage 0: build\n  stage 1: test\n  stage 2: report\n"
        ),
        "{out}"
    );
}

/// A run is recorded for the dashboard with its secrets masked, and not at all
/// with --insecure-secrets or --no-history.
#[test]
fn history_records_masked_runs() {
    let cache = tempfile::tempdir().unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_runthrough"))
            .args(["-C", FIXTURE, "--host", "--no-cache-server", "push"])
            .args([
                "-s",
                "TOKEN=supersecret",
                "--env",
                "FROM_CLI=supersecret-too",
            ])
            .args(extra)
            .env("XDG_CACHE_HOME", cache.path())
            .output()
            .unwrap()
    };
    assert!(run(&[]).status.success());
    assert!(run(&["--insecure-secrets"]).status.success());
    assert!(run(&["--no-history"]).status.success());
    let runs: Vec<_> = std::fs::read_dir(cache.path().join("runthrough/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let log = std::fs::read_to_string(runs[0].join("log.jsonl")).unwrap();
    let meta = std::fs::read_to_string(runs[0].join("meta.json")).unwrap();
    assert!(log.contains("masked ***"), "{log}");
    assert!(log.contains("🏁 job success"), "{log}");
    assert!(!log.contains("supersecret") && !meta.contains("supersecret"));
    assert!(meta.contains("\"TOKEN=***\""), "{meta}");
    // The run's configuration is recorded, values masked and secrets by name.
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["config"]["env"]["FROM_CLI"], "***-too", "{meta}");
    assert_eq!(
        meta["config"]["secrets"],
        serde_json::json!(["TOKEN"]),
        "{meta}"
    );
    let result = std::fs::read_to_string(runs[0].join("result.json")).unwrap();
    assert!(result.contains("\"ok\":true"), "{result}");
}

#[test]
fn push_workflow_succeeds() {
    let (code, out) = runthrough(&["push", "-s", "TOKEN=supersecret"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("composite said 'hello runthrough'"), "{out}");
    assert!(out.contains("matrix n=3 version=1.2.1"), "{out}");
    assert!(out.contains("results success,success"), "{out}");
    assert!(
        out.contains("masked ***") && !out.contains("supersecret"),
        "{out}"
    );
}

/// Workflows under .runthrough/workflows are found next to .github/workflows,
/// unless -W names a directory.
#[test]
fn runthrough_dir_workflows_are_discovered() {
    let (code, out) = runthrough(&["status"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("from .runthrough"), "{out}");
    assert!(out.contains("local-only / rehearse: success"), "{out}");
    let (_, out) = runthrough(&["status", "-W", ".github/workflows"]);
    assert!(!out.contains("from .runthrough"), "{out}");
}

#[test]
fn failing_workflow_runs_failure_steps_and_skips_dependents() {
    let (code, out) = runthrough(&["pull_request"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("node got ab"), "{out}");
    assert!(out.contains("cleanup after failure"), "{out}");
    assert!(out.contains("post sees state ab"), "{out}");
    assert!(out.contains("failing / never: skipped"), "{out}");
}

#[test]
fn reusable_workflow_gets_inputs_secrets_and_returns_outputs() {
    let (code, out) = runthrough(&[
        "workflow_dispatch",
        "-s",
        "TOKEN=supersecret",
        "--input-file",
        "dispatch.input",
        "--actor",
        "octo",
        "--defaultbranch",
        "trunk",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("event=workflow_dispatch actor=octo default=trunk who=file"),
        "{out}"
    );
    assert!(
        out.contains("reusable said hello file flag=on token=***"),
        "{out}"
    );
    assert!(!out.contains("supersecret"), "{out}");
}

#[test]
fn reusable_workflow_cycles_are_refused() {
    let (code, out) = runthrough(&["gollum"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("reusable workflow calls itself: cycle.yml → cycle.yml"),
        "{out}"
    );
    assert!(out.contains("cycle / self: failure"), "{out}");
}

#[test]
fn reusable_workflow_calls_are_checked_against_workflow_call() {
    let (code, out) = runthrough(&[
        "issues",
        "-s",
        "MUST=must-value",
        "-s",
        "GITHUB_TOKEN=token-value",
    ]);
    assert_eq!(code, 1, "{out}");
    for msg in [
        "input 'extra' is not defined in the called workflow",
        "input 'need' is required, but not provided",
        "secret 'other' is not defined in the called workflow",
        "secret 'must' is required, but not provided",
    ] {
        assert!(out.contains(msg), "{msg}\n{out}");
    }
    // A `secrets:` mapping still passes GITHUB_TOKEN on, as on GitHub.
    assert!(
        out.contains("strict got need=yes must=*** token=***"),
        "{out}"
    );
    assert!(out.contains("calls / good: success"), "{out}");
    assert!(out.contains("calls / bad: failure"), "{out}");
}

#[test]
fn workflow_dispatch_inputs_are_validated() {
    let run = |target: &str| {
        runthrough_full(&[
            "workflow_dispatch",
            "-W",
            ".github/dispatch-checks/inputs.yml",
            "--input",
            target,
        ])
    };
    let (code, _, err) = run("target=qa");
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("input 'target' must be one of dev, prod, got 'qa'"),
        "{err}"
    );
    let (code, out, _) = run("dry=true");
    assert_eq!(code, 2, "{out}");
    let (code, out, _) = run("target=prod");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("target=prod dry=false"), "{out}");
}

#[test]
fn problem_matchers_turn_output_into_annotations() {
    let (code, out) = runthrough(&["issue_comment"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("⚠️  src/a.rs:3:7: unused variable"), "{out}");
    assert!(out.contains("❗ src/b.rs:9:1: mismatched types"), "{out}");
    assert!(out.contains("kind=comment"), "{out}");
    // Removed matchers stop matching.
    assert!(!out.contains("❗ src/c.rs"), "{out}");
}

#[test]
fn detect_event_uses_the_first_declared_event() {
    // Pinned to one workflow so fixtures added later cannot change "first".
    let (code, out) = runthrough(&[
        "--detect-event",
        "-W",
        ".github/workflows/caller.yml",
        "-j",
        "info",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("event=workflow_dispatch"), "{out}");
}

#[test]
fn matrix_filter_runs_only_matching_combinations() {
    let (code, out) = runthrough(&["push", "-j", "test", "--matrix", "n:3"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("matrix n=3"), "{out}");
    assert!(!out.contains("matrix n=1"), "{out}");
}

#[test]
fn workflows_in_subdirectories_are_found_unless_no_recurse() {
    let (code, out) = runthrough(&["repository_dispatch"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nested workflow ran"), "{out}");
    let (code, out) = runthrough(&["repository_dispatch", "--no-recurse"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("no workflows triggered by 'repository_dispatch'"),
        "{out}"
    );
}

#[test]
fn output_flags() {
    let (_, out) = runthrough(&["push", "-j", "build", "--quiet"]);
    // Step headers stay; the lines steps print are hidden.
    assert!(out.contains("⭐ Run echo \"composite said"), "{out}");
    assert!(!out.contains("| composite said"), "{out}");
    assert!(out.contains("demo / build: success"), "{out}");

    let (_, out) = runthrough(&["push", "-j", "build", "--log-prefix-job-id"]);
    assert!(
        out.contains("[build] ") && !out.contains("[demo/build]"),
        "{out}"
    );

    let (_, out) = runthrough(&["push", "-j", "build", "--json"]);
    for line in out.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect(line);
        assert!(v.get("time").is_some(), "{line}");
    }

    let (_, out) = runthrough(&[
        "push",
        "-j",
        "build",
        "-s",
        "TOKEN=supersecret",
        "--insecure-secrets",
    ]);
    assert!(out.contains("masked supersecret"), "{out}");
}

#[test]
fn concurrent_jobs_limit_still_completes() {
    let (code, out) = runthrough(&["push", "--concurrent-jobs", "1"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("demo / report: success"), "{out}");
}

#[test]
fn validate_and_strict() {
    let (code, out) = runthrough(&["--validate", "--strict"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("✅") && out.contains("caller.yml"), "{out}");

    let bad = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/strict/bad.yml");
    let (code, _, err) = runthrough_full(&["--validate", "--strict", "-W", bad]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("jobs.x: unknown key 'stesp'"), "{err}");
    let (code, _, err) = runthrough_full(&["--validate", "-W", bad]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn graph_and_informational_output() {
    let (code, out) = runthrough(&["push", "--graph"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("│ build │") && out.contains("⬇"), "{out}");

    let (_, out) = runthrough(&["--list-options"]);
    let opts: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert!(
        opts.as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "matrix" && o["type"] == "stringArray"),
        "{out}"
    );

    let (_, out) = runthrough(&["--man-page"]);
    assert!(out.starts_with(".TH RUNTHROUGH 1"), "{out}");
    assert!(out.contains("\\-\\-concurrent\\-jobs"), "{out}");

    let (_, out) = runthrough(&["--bug-report"]);
    assert!(out.contains("runthrough version:"), "{out}");
}

#[test]
#[ignore = "binds local ports"]
fn cache_and_artifact_servers_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let art = dir.path().join("art");
    let out = bin()
        .args(["-C", FIXTURE, "--host", "check_run", "--cache-server-path"])
        .arg(&cache)
        .arg("--artifact-server-path")
        .arg(&art)
        .args(["--artifact-server-port", "0"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("cache roundtrip ok"), "{out}");
    assert!(out.contains("artifact roundtrip ok"), "{out}");
}

#[test]
fn watch_reruns_after_a_file_changes() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("w.yml"),
        "on: push\njobs:\n  w:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo watched\n",
    )
    .unwrap();
    let mut child = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--host", "--no-cache-server", "--watch"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let wait_for_run = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = 0;
        while Instant::now() < deadline {
            if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
                if l.contains("w.yml / w: success") {
                    seen += 1;
                    if seen == n {
                        return true;
                    }
                }
            }
        }
        false
    };
    assert!(wait_for_run(1), "first run");
    std::fs::write(dir.path().join("changed.txt"), "x").unwrap();
    let rerun = wait_for_run(1);
    let _ = child.kill();
    let _ = child.wait();
    assert!(rerun, "no rerun after a change");
}

/// Without `-e`, merge_group gets a payload for HEAD queued onto the default branch.
#[test]
fn merge_group_event_is_built_from_the_repository() {
    let (code, out) = runthrough(&["merge_group"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("base=true ref=true sha=true base_ref=refs/heads/"),
        "{out}"
    );
}

#[test]
fn local_repository_replaces_remote_actions_and_workflows() {
    let (code, out) = runthrough(&[
        "release",
        "--local-repository",
        "example/greet@v1=.github/actions/greet",
        "--local-repository",
        "example/workflows@v1=.",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("mapped action said hello mapped"), "{out}");
    assert!(
        out.contains("action ref [v1] repo [example/greet]"),
        "{out}"
    );
    assert!(
        out.contains("mapped workflow said hello remote flag=on"),
        "{out}"
    );
}

/// Docker actions, `docker://` steps, service containers and the cache server
/// reached from a container; needs the engine plus local `debian:12-slim` and
/// `pgvector/pgvector:pg16` images.
fn containers_on(engine: &str) {
    containers_with(engine, &[]);
}

/// Container tests publish fixed host ports, so they run one at a time.
static CONTAINER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn containers_with(engine: &str, env: &[(&str, &str)]) {
    let _serial = CONTAINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cmd = bin();
    cmd.envs(env.iter().copied());
    let cache = tempfile::tempdir().unwrap();
    cmd.args(["-C", FIXTURE, "deployment", "--engine", engine])
        .args(["-P", "ubuntu-latest=debian:12-slim", "--cache-server-path"])
        .arg(cache.path());
    if engine == "container" {
        cmd.args(["--container-options", "--dns 1.1.1.1"]);
    }
    let out = cmd.output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "dockerfile action ran as docker with arg docker",
        "dockerfile output hello docker",
        "dockerfile action ran as docker with arg overridden",
        "bare dockerfile action",
        "image step sees blue in",
        "cache server from container: HTTP/1.1 204",
        "docker post sees docker",
        "service db reachable, published as true",
        "service db2 runs its command",
        "toolcache=[/opt/hostedtoolcache] lang=[C.UTF-8] imageos=[ubuntu24]",
        "path=[/opt/added] dashed=[kept]",
        "made below the workspace",
        "containers / actions: success",
        "containers / services: success",
    ] {
        assert!(out.contains(expected), "missing '{expected}':\n{out}");
    }
}

#[test]
#[ignore = "needs Docker"]
fn containers_on_docker() {
    containers_on("docker");
}

#[test]
#[ignore = "needs Apple's container CLI"]
fn containers_on_apple_container() {
    containers_on("container");
}

#[test]
fn github_instance_and_remote_name_shape_the_github_context() {
    // GITHUB_SERVER_URL is always set inside GitHub Actions; --github-instance must win.
    let out = bin()
        .args([
            "-C",
            FIXTURE,
            "--host",
            "--no-cache-server",
            "workflow_dispatch",
        ])
        .args(["-j", "info", "--github-instance", "ghe.example.com"])
        .args(["--remote-name", "no-such-remote"])
        .env("GITHUB_SERVER_URL", "https://github.com")
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    let out = String::from_utf8_lossy(&out.stdout);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "server=https://ghe.example.com api=https://ghe.example.com/api/v3 repo=local/repo"
        ),
        "{out}"
    );
}

#[test]
fn use_gitignore_false_copies_ignored_files() {
    let repo = TempRepo::new();
    std::fs::write(repo.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(repo.path().join("ignored.txt"), "x").unwrap();
    repo.workflow(
        "g.yml",
        "on: push\njobs:\n  g:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: test -f ignored.txt && echo present || echo absent\n",
    );
    let run = |extra: &[&str]| repo.run(&[&["--host", "--no-cache-server"][..], extra].concat());
    assert!(run(&[]).contains("| absent"));
    assert!(run(&["--use-gitignore", "false"]).contains("| present"));
}

#[test]
fn push_before_is_the_remote_branch_and_the_checkout_has_it_pushed() {
    // HEAD is a commit ahead of origin/main: `before` is origin/main, and in the
    // checkout origin/main is HEAD, as after the push on GitHub.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@x"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "--quiet", "-b", "main"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "pushed"]);
    let pushed = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/remotes/origin/main", &pushed]);
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::write(
        root.join(".github/workflows/b.yml"),
        "on: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo \"before=${{ github.event.before }} origin=$(git rev-parse origin/main)\"\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "unpushed"]);
    let head = git(&["rev-parse", "HEAD"]);
    let out = bin()
        .arg("-C")
        .arg(root)
        .args(["--host", "--no-cache-server"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.contains(&format!("before={pushed} origin={head}")),
        "{out}"
    );
}

#[test]
fn host_jobs_run_one_at_a_time() {
    // Like a self-hosted runner, the host runs one job mapped to it at a time:
    // jobs there share the machine (`~/.rustup`, the keychain, ...). `--host`
    // keeps running jobs in parallel.
    let repo = TempRepo::new();
    let job = |id: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo start >> \"$TRACE\"; sleep 1; echo end >> \"$TRACE\"\n")
    };
    repo.workflow(
        "h.yml",
        &format!("on: push\njobs:\n{}{}", job("a"), job("b")),
    );
    let trace = repo.path().join("trace");
    let out = repo.run(&[
        "--no-cache-server",
        "-P",
        "ubuntu-latest=-self-hosted",
        "--env",
        &format!("TRACE={}", trace.display()),
    ]);
    assert!(out.contains("the host runs one job at a time"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&trace).unwrap(),
        "start\nend\nstart\nend\n",
        "{out}"
    );
}

#[test]
fn a_stopped_engine_is_reported_before_any_pull() {
    // With the daemon down, container jobs fail with what to do instead of on
    // their first `docker image pull`; host jobs still run.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "--quiet"])
        .status()
        .unwrap()
        .success());
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::write(
        root.join(".github/workflows/e.yml"),
        "on: push\njobs:\n  c1:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo c1\n  c2:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo c2\n  h:\n    runs-on: self-hosted\n    steps:\n      - run: echo host-ran\n",
    )
    .unwrap();
    let fake = root.join("fake-bin");
    std::fs::create_dir_all(&fake).unwrap();
    let calls = root.join("calls");
    std::fs::write(
        fake.join("docker"),
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho 'Cannot connect to the Docker daemon at unix:///nowhere.sock. Is the docker daemon running?' >&2\nexit 1\n",
            calls.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(fake.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let out = bin()
        .arg("-C")
        .arg(root)
        .arg("--no-cache-server")
        .env("PATH", path)
        .env_remove("DOCKER_HOST")
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(
        all.contains("the docker engine is not running: start Docker Desktop"),
        "{all}"
    );
    assert!(all.contains("Cannot connect to the Docker daemon"), "{all}");
    assert!(all.contains("host-ran"), "{all}");
    assert!(all.contains("/ h: success"), "{all}");
    let calls = std::fs::read_to_string(&calls).unwrap();
    // Each container job probed the daemon; none tried to pull.
    assert!(
        calls.lines().count() == 2
            && calls
                .lines()
                .all(|l| l == "version --format {{.Server.Version}}"),
        "{calls}\n{all}"
    );
}

#[test]
fn simulated_token_job_gets_a_placeholder_token_only_when_none_is_given() {
    let repo = TempRepo::new();
    let job = |id: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"{id} token=${{{{ github.token }}}} secret=${{{{ secrets.GITHUB_TOKEN }}}}\"\n")
    };
    repo.workflow(
        "t.yml",
        &format!("on: push\njobs:\n{}{}", job("sbom"), job("other")),
    );
    let run = |extra: &[&str]| {
        let base = [
            "--host",
            "--no-cache-server",
            "--simulated-token-job",
            "sbom",
        ];
        repo.run(&[&base[..], extra].concat())
    };
    let out = run(&[]);
    let simulated = "ghs_runthroughSimulatedToken0000000000";
    assert!(
        out.contains(&format!("sbom token={simulated} secret={simulated}")),
        "{out}"
    );
    assert!(out.contains("other token= secret="), "{out}");
    // A given token wins (and is masked).
    let out = run(&["-s", "GITHUB_TOKEN=given-token"]);
    assert!(out.contains("sbom token=*** secret=***"), "{out}");
}

#[test]
fn event_inputs_env_act_and_values_from_the_environment() {
    let event = format!("{FIXTURE}/dispatch-event.json");
    let out = bin()
        .args([
            "-C",
            FIXTURE,
            "--host",
            "--no-cache-server",
            "workflow_dispatch",
        ])
        .args([
            "-j", "info", "-e", &event, "--var", "FROMENV", "--env", "FROMENV",
        ])
        .env("FROMENV", "outside")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("who=from-event"), "{out}");
    assert!(
        out.contains("event.inputs.who=from-event var=outside env=outside"),
        "{out}"
    );
    assert!(!out.contains("| not skipped by env.ACT"), "{out}");

    // --input wins over the event payload and shows up in github.event.inputs.
    let (_, out) = runthrough(&[
        "workflow_dispatch",
        "-j",
        "info",
        "-e",
        &event,
        "--input",
        "who=cli",
    ]);
    assert!(out.contains("event.inputs.who=cli"), "{out}");
}

#[test]
fn actrc_supplies_default_arguments() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".actrc"), "--matrix n:3\n-j test\n").unwrap();
    let out = bin()
        .args(["-C", FIXTURE, "--host", "--no-cache-server", "push"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("xdg"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("matrix n=3"), "{out}");
    assert!(
        !out.contains("matrix n=1") && !out.contains("demo / report"),
        "{out}"
    );
}

#[test]
fn schema_errors_fail_without_strict_and_pull_flags_parse() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.yml");
    std::fs::write(
        &bad,
        "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n        uses: x/y@v1\n",
    )
    .unwrap();
    let (code, _, err) = runthrough_full(&["-W", bad.to_str().unwrap()]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("jobs.a.steps[0]: has both 'run' and 'uses'"),
        "{err}"
    );

    for flags in [
        &["--pull=false"][..],
        &["--pull"],
        &["--action-offline-mode"],
    ] {
        let (code, out) = runthrough(&[&["push", "-n"][..], flags].concat());
        assert_eq!(code, 0, "{flags:?}: {out}");
    }
}

#[test]
#[ignore = "binds local ports and fetches actions from github.com"]
fn real_upload_and_download_artifact_v3_and_v4() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["-C", FIXTURE, "--host", "--no-cache-server", "page_build"])
        .arg("--artifact-server-path")
        .arg(dir.path().join("art"))
        .args(["--artifact-server-port", "0", "--action-cache-dir"])
        .arg(dir.path().join("actions"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("artifacts v3=hello v4=hello"), "{out}");
    assert!(out.contains("artifacts / down: success"), "{out}");
}

/// A remote engine (DOCKER_HOST=ssh://…) cannot see host paths; forcing that
/// mode on a local Docker exercises the volume + `docker cp` path.
#[test]
#[ignore = "needs Docker"]
fn containers_on_docker_in_copy_mode() {
    containers_with("docker", &[("RUNTHROUGH_REMOTE_ENGINE", "1")]);

    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("c.yml"),
        "on: push\njobs:\n  c:\n    runs-on: ubuntu-latest\n    steps:\n      - id: m\n        run: echo x > marker.txt && echo \"val=$(cat marker.txt)\" >> \"$GITHUB_OUTPUT\"\n      - run: echo \"output=${{ steps.m.outputs.val }} still=$(cat marker.txt)\"\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args([
            "-P",
            "ubuntu-latest=debian:12-slim",
            "--no-cache-server",
            "--keep",
        ])
        .env("RUNTHROUGH_REMOTE_ENGINE", "1")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("output=x still=x"), "{out}");
    let kept = out
        .lines()
        .find_map(|l| l.split("scratch kept at ").nth(1))
        .expect("kept scratch");
    // Nothing was bind-mounted: the file stayed in the container's volume.
    assert!(!std::path::Path::new(kept.trim())
        .join("workspace/marker.txt")
        .exists());
    let _ = std::fs::remove_dir_all(kept.trim());
}

#[test]
fn context_from_event_runner_env_and_workflow_commands() {
    let event = format!("{FIXTURE}/dispatch-ref.json");
    let out = bin()
        .args([
            "-C",
            FIXTURE,
            "--host",
            "--no-cache-server",
            "workflow_dispatch",
        ])
        .args(["-j", "info", "-e", &event])
        .env("GITHUB_SERVER_URL", "https://example.test")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.contains("ref=refs/heads/feature-x ref_name=feature-x sha=1111111111111111111111111111111111111111 imageos=ubuntu24 perflog=/dev/null retention=0"),
        "{out}"
    );
    assert!(out.contains("server=https://example.test"), "{out}");
    assert!(
        out.contains("the set-env and add-path commands are disabled"),
        "{out}"
    );
    assert!(
        out.contains("unsafe=[] allowed=[yes] az=from azure"),
        "{out}"
    );
}

#[test]
fn runs_on_group_and_persistent_tool_cache() {
    let cache = tempfile::tempdir().unwrap();
    let run = || {
        let out = bin()
            .args(["-C", FIXTURE, "--no-cache-server", "watch"])
            .args(["-P", "my-runners=-self-hosted"])
            .env("XDG_CACHE_HOME", cache.path())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let first = run();
    assert!(first.contains("ran via group"), "{first}");
    assert!(!first.contains("| persisted"), "{first}");
    let second = run();
    assert!(second.contains("| persisted"), "{second}");
    assert!(
        second.contains(
            &cache
                .path()
                .join("runthrough/toolcache")
                .display()
                .to_string()
        ),
        "{second}"
    );
}

#[test]
#[ignore = "needs Docker and network access to ghcr.io"]
fn container_credentials_are_used_to_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("p.yml"),
        "on: push\njobs:\n  p:\n    runs-on: ubuntu-latest\n    container:\n      image: ghcr.io/runthrough-test/does-not-exist:1\n      credentials:\n        username: nobody\n        password: ${{ secrets.REGISTRY_TOKEN }}\n    steps:\n      - run: echo unreachable\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--no-cache-server", "-s", "REGISTRY_TOKEN=not-a-real-token"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.contains("docker login for ghcr.io/runthrough-test/does-not-exist:1 failed"),
        "{out}"
    );
    assert!(!out.contains("not-a-real-token"), "{out}");
}

#[test]
#[ignore = "needs Docker and local debian:12-slim and pgvector/pgvector:pg16 images"]
fn a_job_given_the_engine_socket_reaches_published_ports_on_localhost() {
    // As on GitHub's runner VM, what a job starts through the socket
    // (`docker compose up -p 8080:…`) is on localhost, and the cache server stays reachable.
    let _serial = CONTAINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let published = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "-p",
            "25499:5432",
            "-e",
            "POSTGRES_PASSWORD=pw",
        ])
        .arg("pgvector/pgvector:pg16")
        .output()
        .unwrap();
    assert!(published.status.success(), "{published:?}");
    let id = String::from_utf8_lossy(&published.stdout)
        .trim()
        .to_string();
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("s.yml"),
        "on: push\njobs:\n  s:\n    runs-on: ubuntu-latest\n    steps:\n      - shell: bash\n        run: |\n          for i in $(seq 30); do { echo > /dev/tcp/localhost/25499; } 2>/dev/null && break; sleep 1; done\n          { echo > /dev/tcp/localhost/25499; } 2>/dev/null && echo \"published port on localhost\"\n          u=\"${ACTIONS_CACHE_URL#http://}\"; hp=\"${u%%/*}\"\n          { echo > \"/dev/tcp/${hp%:*}/${hp#*:}\"; } 2>/dev/null && echo \"cache server reachable\"\n",
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--pull=false", "-P", "ubuntu-latest=debian:12-slim"])
        .args(["--container-daemon-socket", "/var/run/docker.sock"])
        .arg("--cache-server-path")
        .arg(cache.path())
        .output()
        .unwrap();
    let _ = Command::new("docker").args(["rm", "-f", &id]).output();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("published port on localhost"), "{out}");
    assert!(out.contains("cache server reachable"), "{out}");
}

#[test]
#[ignore = "needs Docker and a local debian:12-slim image"]
fn daemon_socket_job_gives_the_socket_only_to_the_named_jobs() {
    // `--daemon-socket-job` alone (as in `.actrc`) mounts the default socket into
    // the named job's container and leaves the others without it.
    let _serial = CONTAINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    let job = |id: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: test -S /var/run/docker.sock && echo \"{id} has the socket\" || echo \"{id} has no socket\"\n")
    };
    std::fs::write(
        wf.join("s.yml"),
        format!("on: push\njobs:\n{}{}", job("compose"), job("other")),
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--pull=false", "-P", "ubuntu-latest=debian:12-slim"])
        .args(["--daemon-socket-job", "compose"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("compose has the socket"), "{out}");
    assert!(out.contains("other has no socket"), "{out}");
}

#[test]
#[ignore = "needs Docker and a local debian:12-slim image"]
fn a_failed_job_image_pull_removes_started_services() {
    // Services start before the job image is pulled; a pull failure must fail
    // the job (exit 1, not an internal error) and remove them and their network.
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("p.yml"),
        "on: push\njobs:\n  p:\n    runs-on: ubuntu-latest\n    container: runthrough-test/does-not-exist:1\n    services:\n      idle:\n        image: debian:12-slim\n        entrypoint: sleep\n        command: \"300\"\n    steps:\n      - run: echo unreachable\n",
    )
    .unwrap();
    let child = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--no-cache-server", "--pull=false"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let prefix = format!("runthrough-p-{}-", child.id());
    let out = child.wait_with_output().unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(
        log.contains("image pull runthrough-test/does-not-exist:1 failed"),
        "{log}"
    );
    assert!(log.contains("p.yml / p: failure"), "{log}");
    assert!(!log.contains("| unreachable"), "{log}");
    // `network ls` has no `-a`: it always lists every network.
    for (kind, all) in [("container", "-aq"), ("network", "-q")] {
        let left = Command::new("docker")
            .args([kind, "ls", all, "--filter", &format!("name={prefix}")])
            .output()
            .unwrap();
        assert!(
            left.stdout.is_empty(),
            "{kind} left behind: {left:?}\n{log}"
        );
    }
}

#[test]
fn bad_workflows_are_skipped_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("good.yml"),
        "on: push\njobs:\n  ok:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"objects equal [${{ fromJSON('{}') == fromJSON('{}') }}] arrays differ [${{ fromJSON('[1]') != fromJSON('[1]') }}]\"\n",
    )
    .unwrap();
    std::fs::write(
        wf.join("bad.yml"),
        "on: push\njobs:\n  \"bad id!\":\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo never\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--host", "--no-cache-server"])
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains("job IDs must start with a letter"),
        "{stderr}"
    );
    assert!(stderr.contains("skipping this workflow"), "{stderr}");
    assert!(
        stdout.contains("objects equal [false] arrays differ [true]"),
        "{stdout}"
    );
    assert!(stdout.contains("good.yml / ok: success"), "{stdout}");
}

#[test]
#[ignore = "needs Docker and the mcr.microsoft.com/powershell image"]
fn pwsh_steps_stop_on_errors_and_fail_on_native_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("p.yml"),
        "on: push\njobs:\n  p:\n    runs-on: ubuntu-latest\n    steps:\n      - shell: pwsh\n        continue-on-error: true\n        run: |\n          Write-Output before\n          Get-Item /nonexistent\n          Write-Output after\n      - id: native\n        shell: pwsh\n        continue-on-error: true\n        run: sh -c \"exit 3\"\n      - run: echo \"native outcome ${{ steps.native.outcome }}\"\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["-P", "ubuntu-latest=mcr.microsoft.com/powershell:latest"])
        .args(["--no-cache-server", "--pull=false"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.contains("| before") && !out.contains("| after"),
        "{out}"
    );
    assert!(out.contains("native outcome failure"), "{out}");
}

/// RUNNER_TEMP takes a `./` tar archive (it failed on Docker Desktop's bind mounts,
/// breaking setup-* actions), a non-root user can write to it, Docker actions see
/// its files, and its volume is removed after the job.
#[test]
#[ignore = "needs Docker"]
fn runner_temp_extracts_tar_archives() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("t.yml"),
        "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      \
         - run: |\n          mkdir -p /tmp/src/bin && echo hi > /tmp/src/bin/f\n          \
         tar czf /tmp/a.tgz -C /tmp/src .\n          mkdir \"$RUNNER_TEMP/x\"\n          \
         tar xz --overwrite -C \"$RUNNER_TEMP/x\" -f /tmp/a.tgz\n          \
         echo \"extracted $(cat \"$RUNNER_TEMP/x/bin/f\")\"\n          \
         echo from-job > \"$RUNNER_TEMP/marker\"\n      \
         - uses: docker://debian:12-slim\n        \
         with: {entrypoint: cat, args: \"${{ runner.temp }}/marker\"}\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["-P", "ubuntu-latest=debian:12-slim", "--no-cache-server"])
        .args(["--pull=false", "--container-options", "--user 1234:1234"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("extracted hi"), "{text}");
    assert!(text.contains("| from-job"), "{text}");
    let vols = Command::new("docker")
        .args(["volume", "ls", "-q", "--filter", "name=-temp"])
        .output()
        .unwrap();
    let vols = String::from_utf8_lossy(&vols.stdout);
    assert!(
        !vols.lines().any(|v| v.starts_with("runthrough-t-")),
        "{vols}"
    );
}

/// Images running as a non-root user can still write outputs and the workspace,
/// with bind mounts and in copy mode (where `docker cp` writes as root).
#[test]
#[ignore = "needs Docker"]
fn non_root_container_user_can_write() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("u.yml"),
        "on: push\njobs:\n  u:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - id: w\n        run: |\n          echo \"v=$(id -u)\" >> \"$GITHUB_OUTPUT\"\n          echo data > created.txt\n      - run: echo \"written as ${{ steps.w.outputs.v }}\"\n",
    )
    .unwrap();
    for remote in ["0", "1"] {
        let out = bin()
            .arg("-C")
            .arg(dir.path())
            .args(["-P", "ubuntu-latest=debian:12-slim", "--no-cache-server"])
            .args(["--container-options", "--user 1234:1234"])
            .env("RUNTHROUGH_REMOTE_ENGINE", remote)
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&out.stdout);
        assert!(out.contains("written as 1234"), "copy mode {remote}: {out}");
    }
}

/// A cancelled or timed-out Docker action takes its sibling container with it.
#[test]
#[ignore = "needs Docker"]
fn stopped_docker_actions_leave_no_container() {
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    let sleep = "uses: docker://debian:12-slim, with: {entrypoint: sleep, args: \"300\"}";
    std::fs::write(
        wf.join("d.yml"),
        format!(
            "on: push\njobs:\n  d:\n    runs-on: ubuntu-latest\n    steps:\n      \
             - {{id: bg, background: true, {sleep}}}\n      \
             - run: sleep 2\n      \
             - cancel: bg\n      \
             - {{timeout-minutes: 0.05, {sleep}}}\n"
        ),
    )
    .unwrap();
    let child = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--host", "--no-cache-server"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let prefix = format!("runthrough-step-{}-", child.id());
    let out = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("cancelled"), "{out}");
    assert!(out.contains("timed out"), "{out}");
    let left = Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("name={prefix}")])
        .output()
        .unwrap();
    assert!(left.stdout.is_empty(), "left behind: {left:?}\n{out}");
}

#[test]
fn background_wait_cancel_and_parallel_steps() {
    let started = std::time::Instant::now();
    // failfast (3) must be running, not waiting for a slot, when it is cancelled.
    let (code, out) = runthrough(&["discussion", "--concurrent-jobs", "8"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("before wait '' ''"), "{out}");
    assert!(out.contains("after wait 1234 from-bg success"), "{out}");
    assert!(out.contains("forever cancelled"), "{out}");
    assert!(out.contains("parallel ab"), "{out}");
    assert!(out.contains("bad failure success"), "{out}");
    assert!(out.contains("background / steps: success"), "{out}");
    // A failed background step nobody waited for fails the job at its end.
    assert!(out.contains("next step ran"), "{out}");
    assert!(out.contains("background / unwaited: failure"), "{out}");
    // A cancelled job stops its background steps without failing.
    assert!(
        out.contains("[background/failfast (1)] ❌ job cancelled"),
        "{out}"
    );
    assert!(
        out.contains("[background/failfast (3)] ❌ job cancelled"),
        "{out}"
    );
    // The step waiting for a slot is cancelled at once.
    assert!(
        out.contains("queued cancelled after 0s") || out.contains("queued cancelled after 1s"),
        "{out}"
    );
    assert!(!out.contains("| queued ran"), "{out}");
    // `cancel` stopped `sleep 300`, and `parallel` ran its two 1s steps side by side.
    assert!(started.elapsed().as_secs() < 30, "{out}");
}

#[test]
fn parallel_children_run_pre_hooks_in_the_pre_phase() {
    let dir = tempfile::tempdir().unwrap();
    let action = dir.path().join("act");
    std::fs::create_dir_all(&action).unwrap();
    std::fs::write(
        action.join("action.yml"),
        "name: p\nruns:\n  using: node20\n  pre: pre.js\n  main: main.js\n",
    )
    .unwrap();
    std::fs::write(
        action.join("pre.js"),
        "require('fs').appendFileSync(process.env.GITHUB_STATE, 'from=pre\\n'); console.log('pre hook ran');\n",
    )
    .unwrap();
    std::fs::write(
        action.join("main.js"),
        "console.log('main saw ' + process.env.STATE_from);\n",
    )
    .unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("w.yml"),
        "on: push\njobs:\n  w:\n    runs-on: ubuntu-latest\n    steps:\n      - parallel:\n          - uses: example/pre@v1\n          - run: echo other\n",
    )
    .unwrap();
    let out = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--host", "--no-cache-server", "--local-repository"])
        .arg(format!("example/pre@v1={}", action.display()))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    let pre = out.find("pre hook ran").expect(&out);
    let group = out.find("Run steps in parallel").expect(&out);
    assert!(pre < group, "{out}");
    assert!(out.contains("main saw pre"), "{out}");
    assert_eq!(out.matches("pre hook ran").count(), 1, "{out}");
}

#[test]
fn background_steps_end_when_runthrough_is_killed() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let wf = dir.path().join(".github/workflows");
    std::fs::create_dir_all(&wf).unwrap();
    let pid_file = dir.path().join("pid");
    std::fs::write(
        wf.join("w.yml"),
        format!(
            "on: push\njobs:\n  w:\n    runs-on: ubuntu-latest\n    steps:\n      \
             - background: true\n        run: sleep 300 & echo $! > {}; wait\n      \
             - run: sleep 300\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut child = bin()
        .arg("-C")
        .arg(dir.path())
        .args(["--host", "--no-cache-server"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid = loop {
        match std::fs::read_to_string(&pid_file) {
            Ok(p) if !p.trim().is_empty() => break p.trim().to_string(),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => panic!("the background step never started"),
        }
    };
    let alive = || {
        // A killed process its parent never reaps (PID 1 is `tail` in a job
        // container) stays a zombie, which `kill -0` still finds.
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit_once(") ").is_some_and(|(_, r)| r.starts_with('Z')));
        !zombie
            && Command::new("kill")
                .args(["-0", &pid])
                // "No such process" is the answer we wait for, not an error.
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
    };
    assert!(alive());
    child.kill().unwrap();
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let survived = alive();
    if survived {
        let _ = Command::new("kill").arg(&pid).status();
    }
    assert!(!survived, "the background step outlived runthrough");
}

#[test]
fn concurrency_environment_and_run_name() {
    let (code, out) = runthrough(&["milestone", "--actor", "octo"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("🏷️  concurrency: run by octo"), "{out}");
    // queue: max runs every leg, one at a time.
    assert!(!out.contains("legs overlapped"), "{out}");
    assert!(out.contains("concurrency / queued: success"), "{out}");
    for n in 1..=3 {
        assert!(out.contains(&format!("🌍 environment stage-{n}")), "{out}");
        assert!(
            out.contains(&format!("🔗 environment url https://example.test/p{n}")),
            "{out}"
        );
    }
    // The default queue keeps one pending leg: of three, the middle one is replaced.
    assert_eq!(
        out.matches("concurrency group 'replace' was taken").count(),
        1,
        "{out}"
    );
    assert!(out.contains("concurrency / replaced: cancelled"), "{out}");
    // cancel-in-progress cancels the leg that held the group.
    assert_eq!(out.matches("| leg ").count(), 1, "{out}");
    assert!(out.contains("concurrency / superseded: cancelled"), "{out}");
    // A group an enclosing workflow or calling job holds is a deadlock, cancelled;
    // a called workflow's own `concurrency` counts.
    assert!(
        out.contains("deadlock on concurrency group 'WF-concurrency'"),
        "{out}"
    );
    assert!(out.contains("concurrency / deadlock: cancelled"), "{out}");
    assert!(!out.contains("deadlocked job ran"), "{out}");
    assert!(
        out.contains("deadlock on concurrency group 'concurrency-x'"),
        "{out}"
    );
    assert!(!out.contains("called workflow ran"), "{out}");
    assert!(out.contains("concurrency / calls: cancelled"), "{out}");
}

#[test]
fn event_filters_select_workflows() {
    let event = |json: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("event.json");
        std::fs::write(&path, json).unwrap();
        let (code, out, err) = runthrough_full(&["label", "-e", path.to_str().unwrap()]);
        (code, out + &err)
    };
    let (code, out) = event(r#"{"action": "created", "label": {"name": "bug"}}"#);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("filters / labelled: success"), "{out}");
    let (code, out) = event(r#"{"action": "deleted"}"#);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("filters: not triggered: activity type 'deleted' is not in types"),
        "{out}"
    );
    assert!(out.contains("no workflows triggered by 'label'"), "{out}");
}

#[test]
#[ignore = "needs Docker"]
fn background_steps_in_a_container() {
    let _serial = CONTAINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = bin()
        .args(["-C", FIXTURE, "discussion", "--no-cache-server"])
        .args(["-P", "ubuntu-latest=debian:12-slim"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("after wait 1234 from-bg success"), "{out}");
    assert!(out.contains("parallel ab"), "{out}");
    assert!(out.contains("background / steps: success"), "{out}");
    // SIGTERM reached `sleep 300` inside the container, before the SIGKILL fallback.
    let cancelled = out
        .lines()
        .find(|l| l.contains("Run sleep 300 cancelled"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        cancelled.contains("(0.") || cancelled.contains("(1."),
        "{out}"
    );
    assert!(out.contains("timeout left 0"), "{out}");
}

/// Each line of a multi-line secret is masked, and an `::add-mask::` value stays
/// masked in the jobs that read it through `needs`.
#[test]
fn masks_cover_multiline_secrets_and_needs_outputs() {
    let (code, out) = runthrough(&[
        "-W",
        ".github/confinement/masks.yml",
        "-s",
        "KEY=line-AAA\nline-BBB",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("| pem ***") && out.contains("| got ***"),
        "{out}"
    );
    for leaked in ["line-AAA", "line-BBB", "minted-QQQ"] {
        assert!(!out.contains(leaked), "{leaked} in {out}");
    }
}

/// A checkout `path` that leaves the workspace is refused before anything is
/// deleted or written; one that stays inside after `..` is fine.
#[test]
fn checkout_paths_stay_in_the_workspace() {
    let (code, out) = runthrough(&["-W", ".github/confinement/checkout.yml"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("checkout path '../escape' is not under the workspace"),
        "{out}"
    );
    assert!(
        out.contains("checkout path '/tmp/runthrough-checkout-escape' is not under the workspace"),
        "{out}"
    );
    assert!(out.contains("inside checked out"), "{out}");
    assert!(!std::path::Path::new("/tmp/runthrough-checkout-escape").exists());
}

/// Job-level continue-on-error forgives a failure only: a skipped job stays
/// skipped, and so does the job that needs it.
#[test]
fn continue_on_error_keeps_a_skipped_job_skipped() {
    // No --host: the unmapped runs-on label is what skips the job.
    let out = bin()
        .args([
            "-C",
            FIXTURE,
            "--no-cache-server",
            "-P",
            "ubuntu-latest=-self-hosted",
        ])
        .args(["-W", ".github/confinement/skipped.yml"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("skipped / nowhere: skipped"), "{stdout}");
    assert!(stdout.contains("skipped / after: skipped"), "{stdout}");
    assert!(!stdout.contains("after ran"), "{stdout}");
}

#[test]
fn job_status_functions_see_every_ancestor() {
    // As on GitHub: a failed ancestor two levels up still fails `success()`,
    // and a skipped one fails `success()` without making `failure()` true.
    let repo = TempRepo::new();
    let job = |id: &str, extra: &str, run: &str| {
        format!("  {id}:\n{extra}    runs-on: ubuntu-latest\n    steps:\n      - run: {run}\n")
    };
    repo.workflow(
        "chain.yml",
        &[
            "name: chain\non: push\njobs:\n".to_string(),
            job("a", "", "exit 1"),
            job("b", "    needs: a\n    if: always()\n", "echo b"),
            job("c", "    needs: b\n", "echo c ran"),
            job("d", "    needs: b\n    if: failure()\n", "echo d ran"),
            job("s", "    if: false\n", "echo s"),
            job("t", "    needs: s\n    if: failure()\n", "echo t ran"),
            job(
                "u",
                "    needs: s\n    if: ${{ !failure() && !cancelled() }}\n",
                "echo u ran",
            ),
        ]
        .concat(),
    );
    let out = repo.run(&["--host", "--no-cache-server"]);
    assert!(out.contains("chain / c: skipped"), "{out}");
    assert!(out.contains("chain / d: success"), "{out}");
    assert!(out.contains("| d ran"), "{out}");
    assert!(out.contains("chain / t: skipped"), "{out}");
    // The documented way to run after a skipped need.
    assert!(out.contains("| u ran"), "{out}");
    assert!(
        !out.contains("| c ran") && !out.contains("| t ran"),
        "{out}"
    );
}

#[test]
fn continue_on_error_is_evaluated_per_matrix_leg() {
    // GitHub's `experimental` pattern: the tolerated leg neither fails the job
    // nor cancels its sibling through fail-fast. Leg 1 waits for leg 2 to fail
    // before its second step, which a fail-fast would cancel.
    let repo = TempRepo::new();
    repo.workflow(
        "mx.yml",
        "name: mx\non: push\njobs:\n  m:\n    runs-on: ubuntu-latest\n    continue-on-error: ${{ matrix.experimental == true }}\n    strategy:\n      matrix:\n        v: [1, 2]\n        include:\n          - v: 2\n            experimental: true\n    steps:\n      - if: matrix.v == 2\n        run: touch \"$MARK\"; exit 1\n      - run: for i in $(seq 100); do [ -f \"$MARK\" ] && break; sleep 0.1; done; sleep 1\n      - run: echo leg one done\n  after:\n    needs: m\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo after ran\n",
    );
    let mark = repo.path().join("mark");
    let out = bin()
        .arg("-C")
        .arg(repo.path())
        .args(["--host", "--no-cache-server", "--env"])
        .arg(format!("MARK={}", mark.display()))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("| leg one done"), "{stdout}");
    assert!(stdout.contains("mx / m: success"), "{stdout}");
    assert!(stdout.contains("| after ran"), "{stdout}");
}

#[test]
fn run_name_does_not_print_a_secret() {
    let repo = TempRepo::new();
    repo.workflow(
        "rn.yml",
        "name: rn\nrun-name: run ${{ secrets.LONG }}\non: push\njobs:\n  j:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"long=$LONG\"\n        env:\n          LONG: ${{ secrets.LONG }}\n",
    );
    let out = repo.run(&[
        "--host",
        "--no-cache-server",
        "-s",
        "SHORT=pass",
        "-s",
        "LONG=mypassword123",
    ]);
    assert!(out.contains("rn: run ***"), "{out}");
    assert!(out.contains("| long=***"), "{out}");
    assert!(!out.contains("word123"), "{out}");
}

/// A run closes with one line for all its workflows; jobs that use something a
/// local run does not reproduce say so when they start.
#[test]
fn run_ends_with_a_curtain_line_and_understudy_notes() {
    let (code, out) = runthrough(&["push", "-j", "build"]);
    assert_eq!(code, 0, "{out}");
    let last = out.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("🏁 1 workflow, 1 job: 1 succeeded in "),
        "{out}"
    );

    let repo = TempRepo::new();
    repo.workflow(
        "w.yml",
        "on: push\npermissions: read-all\njobs:\n  w:\n    runs-on: ubuntu-latest\n    environment: prod\n    steps:\n      - run: exit 1\n",
    );
    let out = repo.run(&["--host", "--no-cache-server"]);
    assert!(
        out.contains("ℹ️  permissions are not enforced locally"),
        "{out}"
    );
    assert!(
        out.contains("ℹ️  environment secrets and protection rules are not applied locally"),
        "{out}"
    );
    assert!(
        out.lines()
            .last()
            .unwrap_or_default()
            .starts_with("❌ 1 workflow, 1 job: 1 failed in "),
        "{out}"
    );
}

/// `--committed` checks out the HEAD commit and runs the workflows it has;
/// the default copy says what it adds.
#[test]
fn committed_rehearses_the_head_commit() {
    let repo = TempRepo::new();
    let caller = |extra: &str| {
        format!("on: push\njobs:\n  w:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo \"file=$(cat a.txt)\"; test ! -e new.txt || echo \"new=$(cat new.txt)\"\n{extra}  c:\n    uses: ./.github/workflows/called.yml\n")
    };
    let called = |word: &str| {
        format!("on: workflow_call\njobs:\n  c:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo called={word}\n")
    };
    repo.workflow("w.yml", &caller(""));
    repo.workflow("called.yml", &called("committed"));
    std::fs::write(repo.path().join("a.txt"), "committed").unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "c"]);
    std::fs::write(repo.path().join("a.txt"), "edited").unwrap();
    std::fs::write(repo.path().join("new.txt"), "untracked").unwrap();
    repo.workflow("w.yml", &caller("      - run: echo wf=edited\n"));
    repo.workflow("called.yml", &called("edited"));

    let out = repo.run(&["--host", "--no-cache-server"]);
    assert!(
        out.contains("file=edited") && out.contains("new=untracked"),
        "{out}"
    );
    assert!(
        out.contains("wf=edited") && out.contains("called=edited"),
        "{out}"
    );
    assert!(
        out.contains("(working tree, 4 uncommitted changes; --committed leaves them out)"),
        "{out}"
    );
    let out = repo.run(&["--host", "--no-cache-server", "--committed"]);
    assert!(
        out.contains("file=committed") && !out.contains("new=untracked"),
        "{out}"
    );
    assert!(
        out.contains("called=committed") && !out.contains("=edited"),
        "{out}"
    );
    assert!(out.contains("checked out commit "), "{out}");
}

/// An event that triggers nothing names the events that would, with their commands.
#[test]
fn nothing_triggered_names_other_events() {
    let repo = TempRepo::new();
    repo.workflow(
        "w.yml",
        "on: pull_request\njobs:\n  w:\n    runs-on: ubuntu-latest\n    steps: [{run: 'true'}]\n",
    );
    let out = repo.run(&["--host", "--no-cache-server"]);
    assert!(out.contains("no workflows triggered by 'push'"), "{out}");
    assert!(
        out.contains("  pull_request: 1 workflow → runthrough pull_request"),
        "{out}"
    );
}
