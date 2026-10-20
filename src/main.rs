mod actions;
mod backend;
mod commands;
mod context;
mod dashboard;
mod expr;
mod filters;
mod history;
mod info;
mod markers;
mod matchers;
mod planner;
mod runner;
mod servers;
mod workflow;

use anyhow::{bail, Context, Result};
use clap::Parser;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run GitHub Actions workflows locally.
#[derive(Parser, Debug)]
#[command(name = "runthrough", version, about)]
pub struct Cli {
    /// Event that triggers the workflows (push, pull_request, workflow_dispatch, ...)
    #[arg(default_value = "push")]
    event: String,
    /// Use the first event of the workflows as the triggering event
    #[arg(long)]
    detect_event: bool,
    /// Workflow file or directory [default: .github/workflows and .runthrough/workflows]
    #[arg(short = 'W', long)]
    workflows: Option<PathBuf>,
    /// Do not read workflows from subdirectories of --workflows
    #[arg(long)]
    no_recurse: bool,
    /// Run only this job (plus the jobs it needs)
    #[arg(short, long)]
    job: Option<String>,
    /// Run only matrix combinations matching KEY:VALUE (repeatable)
    #[arg(long = "matrix")]
    matrix: Vec<String>,
    /// List workflows and jobs, then exit
    #[arg(short, long)]
    list: bool,
    /// Draw the job graph of each workflow, then exit
    #[arg(short, long)]
    graph: bool,
    /// Validate the workflows, then exit
    #[arg(long)]
    validate: bool,
    /// Reject keys that are not part of the workflow schema
    #[arg(long)]
    strict: bool,
    /// Show the execution plan without running anything
    #[arg(short = 'n', long)]
    dryrun: bool,
    /// Rerun the workflows whenever a file in the repository changes
    #[arg(short, long)]
    watch: bool,
    /// Serve a web dashboard of recorded runs instead of running workflows
    #[arg(long)]
    dashboard: bool,
    /// Address the dashboard binds to
    #[arg(long, default_value = "127.0.0.1")]
    dashboard_addr: String,
    /// Port the dashboard listens on (0 picks a free port)
    #[arg(long, default_value_t = 7766)]
    dashboard_port: u16,
    /// Do not record this run for the dashboard (never recorded with --insecure-secrets)
    #[arg(long)]
    no_history: bool,
    /// JSON file used as the event payload (`github.event`)
    #[arg(short, long)]
    eventpath: Option<PathBuf>,
    /// Name of the default branch (`github.event.repository.default_branch`)
    #[arg(long)]
    defaultbranch: Option<String>,
    /// User that triggered the event (`github.actor`)
    #[arg(short, long)]
    actor: Option<String>,
    /// Git remote used to derive `github.repository`
    #[arg(long, default_value = "origin")]
    remote_name: String,
    /// Secret as KEY=VALUE, or KEY to read it from the environment
    #[arg(short, long = "secret")]
    secrets: Vec<String>,
    /// File with KEY=VALUE secrets
    #[arg(long, default_value = ".secrets")]
    secret_file: PathBuf,
    /// Print secrets in logs instead of masking them (not recommended)
    #[arg(long)]
    insecure_secrets: bool,
    /// Variable (`vars` context) as KEY=VALUE
    #[arg(long = "var")]
    vars: Vec<String>,
    /// File with KEY=VALUE variables
    #[arg(long, default_value = ".vars")]
    var_file: PathBuf,
    /// Environment variable for every step, as KEY=VALUE
    #[arg(long = "env")]
    envs: Vec<String>,
    /// File with KEY=VALUE environment variables
    #[arg(long, default_value = ".env")]
    env_file: PathBuf,
    /// workflow_dispatch input as KEY=VALUE
    #[arg(long = "input")]
    inputs: Vec<String>,
    /// File with KEY=VALUE workflow_dispatch inputs
    #[arg(long, default_value = ".input")]
    input_file: PathBuf,
    /// Map a runs-on label to an image: -P ubuntu-latest=node:20 (use `-self-hosted` for the host)
    #[arg(short = 'P', long = "platform")]
    platforms: Vec<String>,
    /// Run every job directly on the host instead of in containers
    #[arg(long)]
    host: bool,
    /// Container engine: `docker`, or Apple's `container` CLI (macOS 26+)
    #[arg(long, value_enum, default_value_t = backend::Engine::Docker)]
    engine: backend::Engine,
    /// Maximum number of jobs running at once (default: number of CPUs)
    #[arg(long)]
    concurrent_jobs: Option<usize>,
    /// Bind-mount the repository as the workspace instead of copying it
    #[arg(short, long)]
    bind: bool,
    /// Copy only files git does not ignore into the workspace (false copies ignored files too)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    use_gitignore: bool,
    /// Run the HEAD commit's workflows and check out its files instead of the working tree,
    /// so the run sees what a push would (uncommitted and untracked changes are left out)
    #[arg(long, conflicts_with = "bind")]
    committed: bool,
    /// Run the real actions/checkout instead of copying the local repository
    #[arg(long)]
    no_skip_checkout: bool,
    /// Pull images even if present (`--pull=false` uses local images)
    #[arg(short, long, default_value_t = true, action = clap::ArgAction::Set,
          num_args = 0..=1, default_missing_value = "true")]
    pull: bool,
    /// Rebuild images of Dockerfile actions even if already built
    #[arg(long)]
    rebuild: bool,
    /// Keep job containers and their state between runs and reuse them
    #[arg(short, long)]
    reuse: bool,
    /// Accepted for act compatibility: containers are always removed unless --reuse is set
    #[arg(long)]
    rm: bool,
    /// Never fetch remote actions; use the cache only
    #[arg(long)]
    offline: bool,
    /// Reuse cached actions and present images without updating them; fetch only what is missing
    #[arg(long)]
    action_offline_mode: bool,
    /// Directory caching remote actions
    #[arg(long, alias = "action-cache-path")]
    action_cache_dir: Option<PathBuf>,
    /// Use a local directory for an action or reusable workflow: owner/repo@ref=/path (repeatable)
    #[arg(long)]
    local_repository: Vec<String>,
    /// Container platform, e.g. linux/amd64
    #[arg(long)]
    container_architecture: Option<String>,
    /// Extra options for every job container's `run`, e.g. "--dns 1.1.1.1"
    #[arg(long, allow_hyphen_values = true)]
    container_options: Option<String>,
    /// Run job containers in privileged mode
    #[arg(long)]
    privileged: bool,
    /// User namespace for job containers
    #[arg(long)]
    userns: Option<String>,
    /// Kernel capability to add to job containers (repeatable)
    #[arg(long)]
    container_cap_add: Vec<String>,
    /// Kernel capability to drop from job containers (repeatable)
    #[arg(long)]
    container_cap_drop: Vec<String>,
    /// Network for job, service and action containers
    #[arg(long)]
    network: Option<String>,
    /// Host path of the container engine socket to mount at /var/run/docker.sock in job containers (`-`: none)
    #[arg(long)]
    container_daemon_socket: Option<PathBuf>,
    /// Mount the engine socket only into this job's container (repeatable; the socket defaults to /var/run/docker.sock)
    #[arg(long)]
    daemon_socket_job: Vec<String>,
    /// Give this job a placeholder GITHUB_TOKEN when none is set (repeatable), for actions that only require one; GitHub's API rejects it
    #[arg(long)]
    simulated_token_job: Vec<String>,
    /// Directory where the artifact server stores files; the server runs only when set
    #[arg(long)]
    artifact_server_path: Option<PathBuf>,
    /// Address the artifact server binds to
    #[arg(long)]
    artifact_server_addr: Option<String>,
    /// Port the artifact server listens on (0 picks a free port)
    #[arg(long, default_value_t = 34567)]
    artifact_server_port: u16,
    /// Disable the cache server used by actions/cache
    #[arg(long)]
    no_cache_server: bool,
    /// Directory where the cache server stores caches
    #[arg(long)]
    cache_server_path: Option<PathBuf>,
    /// Address the cache server binds to
    #[arg(long)]
    cache_server_addr: Option<String>,
    /// Port the cache server listens on (0 picks a free port)
    #[arg(long, default_value_t = 0)]
    cache_server_port: u16,
    /// URL jobs use to reach the cache server, when it is behind a proxy (no trailing slash)
    #[arg(long)]
    cache_server_external_url: Option<String>,
    /// GitHub Enterprise Server host name (default: github.com)
    #[arg(long, default_value = "github.com")]
    github_instance: String,
    /// With --github-instance, fetch this action from github.com instead (repeatable)
    #[arg(long)]
    replace_ghe_action_with_github_com: Vec<String>,
    /// Token for actions fetched from github.com through --replace-ghe-action-with-github-com
    #[arg(long)]
    replace_ghe_action_token_with_github_com: Option<String>,
    /// Keep each job's scratch directory after the run
    #[arg(long)]
    keep: bool,
    /// Repository root
    #[arg(short = 'C', long, default_value = ".")]
    directory: PathBuf,
    /// Hide the output of steps
    #[arg(short, long)]
    quiet: bool,
    /// Print logs as JSON lines
    #[arg(long)]
    json: bool,
    /// Prefix log lines with the job ID only instead of workflow and job name
    #[arg(long)]
    log_prefix_job_id: bool,
    /// Verbose output (debug messages, skipped steps, commands)
    #[arg(short, long)]
    verbose: bool,
    /// Print system information for a bug report, then exit
    #[arg(long)]
    bug_report: bool,
    /// Install a git pre-push hook that runs the push workflows on the commit being pushed, then exit
    #[arg(long)]
    install_hook: bool,
    /// Print a man page, then exit
    #[arg(long)]
    man_page: bool,
    /// Print the options as JSON, then exit
    #[arg(long)]
    list_options: bool,
}

/// How `KEY` without `=VALUE` is resolved.
#[derive(Clone, Copy, PartialEq)]
enum Missing {
    /// Not allowed.
    Reject,
    /// Read from the environment.
    FromEnv,
    /// Read from the environment, else ask on the terminal without echo (secrets).
    FromEnvOrPrompt,
}

fn parse_kv(items: &[String], missing: Missing) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    for item in items {
        match item.split_once('=') {
            Some((k, v)) => {
                out.insert(k.to_string(), v.to_string());
            }
            None if missing == Missing::Reject => bail!("expected KEY=VALUE, got '{item}'"),
            None => {
                let v = match std::env::var(item) {
                    Ok(v) => v,
                    Err(_) if missing == Missing::FromEnvOrPrompt => prompt_secret(item)?,
                    Err(_) => bail!("environment variable {item} is not set"),
                };
                out.insert(item.clone(), v);
            }
        }
    }
    Ok(out)
}

/// Ask for a secret on the controlling terminal with echo turned off.
fn prompt_secret(name: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .with_context(|| {
            format!("secret {name} is not set and there is no terminal to ask for it")
        })?;
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(
                tty.try_clone()
                    .map(std::process::Stdio::from)
                    .unwrap_or(std::process::Stdio::null()),
            )
            .status()
    };
    write!(&tty, "Provide value for '{name}': ")?;
    stty("-echo")?;
    let mut line = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut line);
    stty("echo")?;
    writeln!(&tty)?;
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Arguments from `.actrc` files (XDG config, home, current directory), one per
/// line; `--flag value` on one line becomes two arguments.
fn actrc_args() -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let files = [
        xdg.map(|d| d.join("act").join("actrc")),
        home.map(|h| h.join(".actrc")),
        Some(PathBuf::from(".actrc")),
    ];
    let mut args = Vec::new();
    for f in files.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match line.split_once(char::is_whitespace) {
                Some((flag, value)) if flag.starts_with('-') && !flag.contains('=') => {
                    args.push(flag.to_string());
                    args.push(value.trim().to_string());
                }
                _ => args.push(line.to_string()),
            }
        }
    }
    args
}

/// Read a dotenv-style file; a missing file yields an empty map.
fn read_kv_file(path: &Path) -> Result<IndexMap<String, String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(IndexMap::new());
    };
    let mut out = IndexMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            bail!("{}: expected KEY=VALUE, got '{line}'", path.display());
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(|s| s.replace("\\n", "\n"))
            .or_else(|| {
                v.strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .map(String::from)
            })
            .unwrap_or_else(|| v.to_string());
        out.insert(k.trim().to_string(), v);
    }
    Ok(out)
}

fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("runthrough")
}

fn main() {
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let rc: Vec<std::ffi::OsString> = actrc_args().into_iter().map(Into::into).collect();
    argv.splice(1..1, rc);
    let cli = Cli::parse_from(argv);
    if cli.man_page {
        print!("{}", info::man_page());
        return;
    }
    if cli.list_options {
        println!("{}", info::list_options());
        return;
    }
    if cli.bug_report {
        print!("{}", info::bug_report(&cli.engine, &cache_base()));
        return;
    }
    if cli.install_hook {
        match info::install_hook(&cli.directory) {
            Ok(hook) => println!(
                "installed {}: each git push first runs `runthrough --committed --quiet push` (skip it once with git push --no-verify)",
                hook.display()
            ),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    if cli.dashboard {
        if let Err(e) = dashboard::serve(cache_base(), &cli.dashboard_addr, cli.dashboard_port) {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
    let result = if cli.watch {
        watch(&cli)
    } else {
        run(&cli).map(|o| o.ok)
    };
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}

/// Run once, then again every time the repository's files change.
fn watch(cli: &Cli) -> Result<bool> {
    let root = cli.directory.canonicalize()?;
    let mut last: Option<Outcome> = None;
    loop {
        // Taken before the run, so a change made while it runs triggers the next one.
        let before = info::fingerprint(&root);
        match run(cli) {
            Ok(outcome) => {
                if let Some(line) = last.as_ref().and_then(|l| outcome.since(l)) {
                    println!("{line}");
                }
                last = Some(outcome);
            }
            Err(e) => eprintln!("error: {e:#}"),
        }
        eprintln!(
            "👀 watching {} for changes (Ctrl-C to stop)",
            root.display()
        );
        loop {
            let started = std::time::Instant::now();
            if info::fingerprint(&root) != before {
                break;
            }
            std::thread::sleep(watch_delay(started.elapsed()));
        }
    }
}

/// The pause between two `--watch` checks: half a second, or ten times what a
/// check took, so a large repository never keeps more than a tenth of a core busy.
fn watch_delay(check: std::time::Duration) -> std::time::Duration {
    std::time::Duration::from_millis(500).max(check * 10)
}

/// Load every workflow; files that fail are reported and skipped (as act does),
/// and the run fails only when none loads. The flag says whether any failed.
/// With `head` (`--committed`: the repository at `root` as its HEAD commit
/// has it), files under `root` are read from there, keeping their names.
fn load_workflows(
    cli: &Cli,
    wf_paths: &[PathBuf],
    root: &Path,
    head: Option<&Path>,
) -> Result<(Vec<workflow::Workflow>, bool)> {
    let mut workflows = Vec::new();
    let mut errors = Vec::new();
    let mut files = Vec::new();
    for p in wf_paths {
        let (from, to) = match (head, p.strip_prefix(root)) {
            (Some(head), Ok(rel)) => (head.join(rel), p.clone()),
            _ => (p.clone(), p.clone()),
        };
        // A missing default directory is reported below as no workflow files.
        if cli.workflows.is_none() && !from.is_dir() {
            continue;
        }
        for f in workflow::discover(&from, !cli.no_recurse)? {
            let name = match f.strip_prefix(&from) {
                Ok(rel) if !rel.as_os_str().is_empty() => to.join(rel),
                _ => to.clone(),
            };
            files.push((f, name));
        }
    }
    for (f, name) in files {
        match workflow::Workflow::load_as(&f, &name).and_then(|wf| {
            if cli.strict {
                workflow::strict_check(&f)?;
            }
            Ok(wf)
        }) {
            Ok(wf) => workflows.push(wf),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    if workflows.is_empty() {
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        let dirs: Vec<_> = wf_paths.iter().map(|p| p.display().to_string()).collect();
        if cli.workflows.is_none() {
            bail!(
                "no workflow files found in {} (runthrough reads .github/workflows, and .runthrough/workflows for workflows run only locally)",
                dirs.join(", ")
            );
        }
        bail!("no workflow files found in {}", dirs.join(", "));
    }
    for e in &errors {
        eprintln!("error: {e}\n  (skipping this workflow)");
    }
    Ok((workflows, !errors.is_empty()))
}

/// What to try when `event` triggers nothing: up to three events the workflows
/// do declare, most workflows first, each with its command; and, when some
/// workflow declares `event` but its filters rejected the payload, how to skip them.
fn trigger_cues(workflows: &[workflow::Workflow], event: &str) -> Vec<String> {
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for wf in workflows {
        // A reusable workflow only runs through its caller.
        for e in wf.events().into_iter().filter(|e| e != "workflow_call") {
            *counts.entry(e).or_default() += 1;
        }
    }
    let filtered = counts.shift_remove(event).is_some();
    let mut events: Vec<(String, usize)> = counts.into_iter().collect();
    events.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut cues: Vec<String> = events
        .into_iter()
        .take(3)
        .map(|(e, n)| {
            let workflows = if n == 1 { "workflow" } else { "workflows" };
            format!("{e}: {n} {workflows} → runthrough {e}")
        })
        .collect();
    if filtered {
        cues.push(format!(
            "-W <file> or -j <job> runs a workflow without its on.{event} filters"
        ));
    }
    cues
}

/// Where to read workflows from: `-W` alone, or by default `.github/workflows`
/// plus `.runthrough/workflows` (laid out the same way, for workflows meant
/// only for local runs). A missing default directory is skipped unless both are.
fn workflow_paths(cli: &Cli, root: &Path) -> Vec<PathBuf> {
    if let Some(w) = &cli.workflows {
        return vec![root.join(w)];
    }
    let defaults = [".github/workflows", ".runthrough/workflows"].map(|d| root.join(d));
    let found: Vec<_> = defaults.iter().filter(|p| p.is_dir()).cloned().collect();
    if found.is_empty() {
        vec![defaults[0].clone()]
    } else {
        found
    }
}

/// How a run ended: whether every job succeeded or was skipped, each job's
/// result as `wf / job`, and the wall time.
struct Outcome {
    ok: bool,
    jobs: Vec<(String, runner::JobResult)>,
    secs: f64,
}

impl Outcome {
    /// A run that ran no job (list, plan, validate, nothing triggered).
    fn no_jobs(ok: bool) -> Outcome {
        Outcome {
            ok,
            jobs: Vec::new(),
            secs: 0.0,
        }
    }

    /// The `--watch` line comparing this run with `last`; none when either ran no job.
    fn since(&self, last: &Outcome) -> Option<String> {
        if self.jobs.is_empty() || last.jobs.is_empty() {
            return None;
        }
        let changed: Vec<(String, &str, &str)> = self
            .jobs
            .iter()
            .filter_map(|(job, now)| {
                let (_, before) = last.jobs.iter().find(|(j, _)| j == job)?;
                (before != now).then(|| (job.clone(), before.as_str(), now.as_str()))
            })
            .collect();
        Some(markers::since_last(&changed, self.secs, last.secs))
    }
}

fn run(cli: &Cli) -> Result<Outcome> {
    let started = std::time::Instant::now();
    let root = cli
        .directory
        .canonicalize()
        .with_context(|| format!("{}", cli.directory.display()))?;
    let head = cli
        .committed
        .then(|| actions::export_head(&root))
        .transpose()?;
    let wf_paths = workflow_paths(cli, &root);
    let (workflows, load_failed) =
        load_workflows(cli, &wf_paths, &root, head.as_ref().map(|d| d.path()))?;

    if cli.validate {
        for wf in &workflows {
            planner::stages(wf, None).with_context(|| format!("{}", wf.path.display()))?;
            println!("✅ {}", wf.path.display());
        }
        return Ok(Outcome::no_jobs(!load_failed));
    }

    if cli.list {
        println!(
            "{:<6} {:<24} {:<32} {:<28} EVENTS",
            "STAGE", "JOB ID", "JOB NAME", "WORKFLOW"
        );
        for wf in &workflows {
            for (i, stage) in planner::stages(wf, None)?.iter().enumerate() {
                for id in stage {
                    let name = wf.jobs[id].name.clone().unwrap_or_else(|| id.clone());
                    let file = wf.path.file_name().unwrap_or_default().to_string_lossy();
                    println!(
                        "{:<6} {:<24} {:<32} {:<28} {}",
                        i,
                        id,
                        name,
                        file,
                        wf.events().join(",")
                    );
                }
            }
        }
        return Ok(Outcome::no_jobs(true));
    }

    let event_name = if cli.detect_event {
        workflows
            .iter()
            .find_map(|wf| wf.events().into_iter().next())
            .context("no workflow declares an event")?
    } else {
        cli.event.clone()
    };

    let mut event: serde_json::Value = match &cli.eventpath {
        Some(p) => serde_json::from_str(
            &std::fs::read_to_string(p).with_context(|| format!("{}", p.display()))?,
        )
        .with_context(|| format!("parsing {}", p.display()))?,
        None => serde_json::json!({}),
    };
    let mut repo = context::RepoInfo::detect(&root, &cli.remote_name);
    if let Some(a) = &cli.actor {
        repo.actor = a.clone();
    }
    repo.host = cli.github_instance.clone();
    if cli.eventpath.is_none() && event_name == "push" {
        if let Some(push) = repo.push_event(&cli.remote_name) {
            event = push;
        }
    }
    let branch = cli
        .defaultbranch
        .clone()
        .unwrap_or_else(|| repo.default_branch.clone());
    if cli.eventpath.is_none() && event_name == "merge_group" {
        event = repo.merge_group_event(&cli.remote_name, &branch);
    }
    repo.fill_event_repository(&mut event, &branch);

    let selected = select_workflows(cli, &workflows, &wf_paths, &event_name, &event, &repo);
    if selected.is_empty() {
        match &cli.job {
            Some(j) => bail!("job '{j}' not found in any workflow"),
            None => {
                println!("no workflows triggered by '{event_name}'");
                for cue in trigger_cues(&workflows, &event_name) {
                    println!("  {cue}");
                }
                return Ok(Outcome::no_jobs(true));
            }
        }
    }

    if cli.graph {
        for wf in &selected {
            print!(
                "{}",
                info::graph(wf, &planner::stages(wf, cli.job.as_deref())?)
            );
        }
        return Ok(Outcome::no_jobs(true));
    }

    if cli.dryrun {
        for wf in &selected {
            println!("{} ({})", wf.display_name(), wf.path.display());
            for (i, stage) in planner::stages(wf, cli.job.as_deref())?.iter().enumerate() {
                println!("  stage {i}: {}", stage.join(", "));
            }
        }
        return Ok(Outcome::no_jobs(true));
    }

    let given = Given::read(cli, &root)?;
    let matrix = matrix_filter(cli)?;
    let local_repos = local_repos(cli, &root)?;

    if cli.bind && !cli.host && cli.engine.is_remote() {
        bail!("--bind needs a local engine; DOCKER_HOST points to a remote one");
    }
    let github_token = given
        .secrets
        .get("GITHUB_TOKEN")
        .filter(|t| !t.is_empty())
        .cloned();
    let base = cache_base();
    let concurrency = cli.concurrent_jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    let servers = servers::Servers::start(cli, &base, cli.engine, cli.host)?;
    let history = (!cli.no_history && !cli.insecure_secrets)
        .then(|| start_history(cli, &base, &repo, &event_name, &selected, &servers, &given))
        .flatten();
    let cfg = runner::Config {
        repo,
        event_name,
        event,
        secrets: given.secrets,
        vars: given.vars,
        env: given.env,
        inputs: given.inputs,
        platforms: given.platforms,
        force_host: cli.host,
        engine: cli.engine,
        copy_mode: !cli.host && cli.engine.is_remote(),
        bind: cli.bind,
        use_gitignore: cli.use_gitignore,
        committed: head.as_ref().map(|d| d.path().to_path_buf()),
        skip_checkout: !cli.no_skip_checkout,
        // Offline mode keeps present images; missing ones are still pulled.
        pull: cli.pull && !cli.action_offline_mode,
        rebuild: cli.rebuild,
        offline: cli.offline,
        action_cache: cli
            .action_cache_dir
            .clone()
            .unwrap_or_else(|| base.join("actions")),
        reuse_dir: cli.reuse.then(|| base.join("reuse")),
        objects: Arc::default(),
        tool_cache: base.join("toolcache"),
        local_repos,
        github: actions::GithubHost {
            instance: cli.github_instance.clone(),
            replace_with_github_com: cli
                .replace_ghe_action_with_github_com
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            github_com_token: cli.replace_ghe_action_token_with_github_com.clone(),
            token: github_token,
        },
        container_platform: cli.container_architecture.clone(),
        container_args: container_args(cli),
        engine_socket: engine_socket(cli),
        socket_jobs: cli.daemon_socket_job.clone(),
        simulated_token_jobs: cli.simulated_token_job.clone(),
        network: cli.network.clone(),
        matrix,
        shared: Arc::new(runner::RunShared::new(concurrency.max(1), history)),
        frame: runner::CallFrame::default(),
        servers,
        keep: cli.keep,
        verbose: cli.verbose,
        quiet: cli.quiet,
        json: cli.json,
        log_prefix_job_id: cli.log_prefix_job_id,
        insecure_secrets: cli.insecure_secrets,
    };
    // Workflows triggered by the same event run side by side, as in act;
    // --concurrent-jobs still bounds the jobs across all of them.
    let results: Vec<Result<(bool, runner::JobResults)>> = std::thread::scope(|s| {
        let handles: Vec<_> = selected
            .iter()
            .map(|wf| {
                let cfg = &cfg;
                let job = cli.job.as_deref();
                s.spawn(move || runner::run_workflow(wf, cfg, job))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Ok((false, Vec::new()))))
            .collect()
    });
    let mut ok = !load_failed;
    let mut error = None;
    let mut jobs = Vec::new();
    for (wf, r) in selected.iter().zip(results) {
        match r {
            Ok((r, results)) => {
                ok &= r;
                jobs.extend(
                    results
                        .into_iter()
                        .map(|(id, result)| (format!("{} / {id}", wf.display_name()), result)),
                );
            }
            Err(e) => error = error.or(Some(e)),
        }
    }
    if let Some(h) = &cfg.shared.history {
        h.finish(ok && error.is_none());
    }
    if let Some(e) = error {
        return Err(e);
    }
    let outcome = Outcome {
        ok,
        jobs,
        secs: started.elapsed().as_secs_f64(),
    };
    print_curtain(cli, selected.len(), &outcome, cfg.shared.history.as_deref());
    Ok(outcome)
}

/// The run's closing line (`markers::curtain`), or one JSON object with `--json`.
fn print_curtain(
    cli: &Cli,
    workflows: usize,
    outcome: &Outcome,
    history: Option<&history::Recorder>,
) {
    let url = history.filter(|_| cli.dashboard_port != 0).map(|h| {
        format!(
            "http://{}:{}/#/run/{}",
            cli.dashboard_addr,
            cli.dashboard_port,
            h.id()
        )
    });
    let results: Vec<&str> = outcome.jobs.iter().map(|(_, r)| r.as_str()).collect();
    if cli.json {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        println!(
            "{}",
            serde_json::json!({
                "time": now,
                "ok": outcome.ok,
                "workflows": workflows,
                "jobs": outcome.jobs.iter().map(|(j, r)| serde_json::json!({"job": j, "result": r})).collect::<Vec<_>>(),
                "secs": outcome.secs,
                "url": url,
            })
        );
    } else {
        println!(
            "{}",
            markers::curtain(
                outcome.ok,
                workflows,
                &results,
                outcome.secs,
                url.as_deref()
            )
        );
    }
}

/// Workflows to run for `event_name`: those it triggers and whose `on.<event>`
/// filters pass (an explicit job or a single workflow file skips both), with
/// the `-j` job if one is given.
fn select_workflows<'w>(
    cli: &Cli,
    workflows: &'w [workflow::Workflow],
    wf_paths: &[PathBuf],
    event_name: &str,
    event: &serde_json::Value,
    repo: &context::RepoInfo,
) -> Vec<&'w workflow::Workflow> {
    let explicit = cli.job.is_some() || wf_paths.iter().any(|p| p.is_file());
    workflows
        .iter()
        .filter(|wf| explicit || wf.events().iter().any(|e| e == event_name))
        .filter(|wf| {
            if explicit {
                return true;
            }
            let reason = filters::skip_reason(&wf.on, event_name, event, repo);
            if let (Some(r), false) = (&reason, cli.quiet) {
                eprintln!("{}: not triggered: {r}", wf.display_name());
            }
            reason.is_none()
        })
        .filter(|wf| cli.job.as_ref().is_none_or(|j| wf.jobs.contains_key(j)))
        .collect()
}

/// What the run is given: each from its file (`.secrets`, `.vars`, `.env`,
/// `.input`), overridden by the flags.
struct Given {
    secrets: IndexMap<String, String>,
    vars: IndexMap<String, String>,
    env: IndexMap<String, String>,
    inputs: IndexMap<String, String>,
    platforms: IndexMap<String, String>,
}

impl Given {
    fn read(cli: &Cli, root: &Path) -> Result<Given> {
        let file = |p: &PathBuf| read_kv_file(&under(root, p));
        let mut secrets = file(&cli.secret_file)?;
        secrets.extend(parse_kv(&cli.secrets, Missing::FromEnvOrPrompt)?);
        let mut vars = file(&cli.var_file)?;
        vars.extend(parse_kv(&cli.vars, Missing::FromEnv)?);
        let mut env = file(&cli.env_file)?;
        env.extend(parse_kv(&cli.envs, Missing::FromEnv)?);
        let mut inputs = file(&cli.input_file)?;
        inputs.extend(parse_kv(&cli.inputs, Missing::Reject)?);
        let platforms = parse_kv(&cli.platforms, Missing::Reject)?;
        Ok(Given {
            secrets,
            vars,
            env,
            inputs,
            platforms,
        })
    }
}

/// `p`, relative to `root` unless absolute.
fn under(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// `--matrix KEY:VALUE` flags: key → accepted values.
fn matrix_filter(cli: &Cli) -> Result<IndexMap<String, Vec<String>>> {
    let mut matrix: IndexMap<String, Vec<String>> = IndexMap::new();
    for m in &cli.matrix {
        let (k, v) = m
            .split_once(':')
            .with_context(|| format!("expected --matrix KEY:VALUE, got '{m}'"))?;
        matrix.entry(k.to_string()).or_default().push(v.to_string());
    }
    Ok(matrix)
}

/// Extra `run` options for every job container.
fn container_args(cli: &Cli) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if cli.privileged {
        args.push("--privileged".into());
    }
    if let Some(u) = &cli.userns {
        args.push(format!("--userns={u}"));
    }
    for c in &cli.container_cap_add {
        args.push(format!("--cap-add={c}"));
    }
    for c in &cli.container_cap_drop {
        args.push(format!("--cap-drop={c}"));
    }
    if let Some(o) = &cli.container_options {
        args.extend(o.split_whitespace().map(String::from));
    }
    args
}

/// The engine socket to mount into jobs. `-` disables the mount, as in act
/// (runthrough mounts nothing by default); `--daemon-socket-job` alone mounts
/// the default socket into those jobs.
fn engine_socket(cli: &Cli) -> Option<PathBuf> {
    match &cli.container_daemon_socket {
        Some(s) if s.as_os_str() == "-" => None,
        Some(s) => Some(s.clone()),
        None if !cli.daemon_socket_job.is_empty() => Some("/var/run/docker.sock".into()),
        None => None,
    }
}

/// `--local-repository owner/repo@ref=PATH`: lowercase `owner/repo@ref` → directory.
fn local_repos(cli: &Cli, root: &Path) -> Result<IndexMap<String, PathBuf>> {
    let mut local_repos = IndexMap::new();
    for spec in &cli.local_repository {
        let (k, v) = spec.split_once('=').with_context(|| {
            format!("expected --local-repository owner/repo@ref=PATH, got '{spec}'")
        })?;
        // act also accepts `https://github.com/owner/repo@ref`.
        let k = k
            .strip_prefix("https://")
            .or_else(|| k.strip_prefix("http://"))
            .and_then(|rest| rest.split_once('/').map(|(_, path)| path))
            .unwrap_or(k);
        local_repos.insert(k.to_ascii_lowercase(), under(root, Path::new(v)));
    }
    Ok(local_repos)
}

/// Start recording the run for `--dashboard`, with its masked argv, the jobs
/// by stage and what it was given (secrets by name only). History is a
/// convenience: a run goes on without it.
fn start_history(
    cli: &Cli,
    base: &Path,
    repo: &context::RepoInfo,
    event_name: &str,
    selected: &[&workflow::Workflow],
    servers: &servers::Servers,
    given: &Given,
) -> Option<Arc<history::Recorder>> {
    let masks: Vec<String> = given
        .secrets
        .values()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    let argv: Vec<String> = std::env::args()
        .map(|a| commands::mask(&a, &masks))
        .collect();
    let workflows: Vec<serde_json::Value> = selected
        .iter()
        .map(|wf| {
            let stages = planner::stages(wf, cli.job.as_deref()).unwrap_or_default();
            let jobs: Vec<serde_json::Value> = stages
                .iter()
                .enumerate()
                .flat_map(|(i, stage)| {
                    stage.iter().map(move |id| {
                        let job = &wf.jobs[id];
                        serde_json::json!({"id": id, "name": job.name, "stage": i, "needs": job.needs()})
                    })
                })
                .collect();
            serde_json::json!({"name": wf.display_name(), "file": wf.path, "jobs": jobs})
        })
        .collect();
    let mut meta = serde_json::Map::new();
    meta.insert("argv".into(), serde_json::json!(argv));
    meta.insert("root".into(), serde_json::json!(repo.root));
    meta.insert("repository".into(), serde_json::json!(repo.repository));
    meta.insert("ref_name".into(), serde_json::json!(repo.ref_name));
    meta.insert("sha".into(), serde_json::json!(repo.sha));
    meta.insert("event".into(), serde_json::json!(event_name));
    meta.insert(
        "version".into(),
        serde_json::json!(env!("CARGO_PKG_VERSION")),
    );
    meta.insert("committed".into(), serde_json::json!(cli.committed));
    meta.insert(
        "engine".into(),
        serde_json::json!(if cli.host {
            "host"
        } else {
            cli.engine.program()
        }),
    );
    meta.insert("workflows".into(), serde_json::json!(workflows));
    meta.insert("servers".into(), servers.ports());
    // What the run was given, for the dashboard: secrets by name only.
    let masked = |kv: &IndexMap<String, String>| -> IndexMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.clone(), commands::mask(v, &masks)))
            .collect()
    };
    meta.insert(
        "config".into(),
        serde_json::json!({
            "env": masked(&given.env),
            "vars": masked(&given.vars),
            "inputs": masked(&given.inputs),
            "secrets": given.secrets.keys().collect::<Vec<_>>(),
            "platforms": given.platforms,
        }),
    );
    // History is a convenience: a run goes on without it.
    history::Recorder::start(base, meta)
        .inspect_err(|e| eprintln!("⚠️  run history disabled: {e:#}"))
        .ok()
        .map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn trigger_cues_name_declared_events() {
        let dir = tempfile::tempdir().unwrap();
        let load = |name: &str, on: &str| {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                format!("on: {on}\njobs:\n  a:\n    runs-on: x\n    steps: [{{run: a}}]\n"),
            )
            .unwrap();
            workflow::Workflow::load_as(&path, &path).unwrap()
        };
        let workflows = [
            load("a.yml", "[pull_request, push]"),
            load("b.yml", "pull_request"),
            load("c.yml", "workflow_call"),
            load("d.yml", "[release, schedule, label]"),
        ];
        assert_eq!(
            trigger_cues(&workflows, "issues"),
            [
                "pull_request: 2 workflows → runthrough pull_request",
                "label: 1 workflow → runthrough label",
                "push: 1 workflow → runthrough push",
            ]
        );
        assert_eq!(
            trigger_cues(&workflows, "push").last().unwrap(),
            "-W <file> or -j <job> runs a workflow without its on.push filters"
        );
    }

    #[test]
    fn watch_checks_take_at_most_a_tenth_of_the_time() {
        assert_eq!(
            watch_delay(Duration::from_millis(5)),
            Duration::from_millis(500)
        );
        for took in [80, 300, 2000].map(Duration::from_millis) {
            let delay = watch_delay(took);
            assert!(took.as_secs_f64() / (took + delay).as_secs_f64() <= 0.1);
        }
    }
}
