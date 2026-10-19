//! Workflow execution: stages of jobs, matrix fan-out, and step execution.

use crate::actions::{self, ActionDef, ActionRef, GithubHost};
use crate::backend::{self, split_args, Backend, ContainerSpec, Engine};
use crate::commands::{self, Command};
use crate::context::RepoInfo;
use crate::expr::{self, Ctx};
use crate::history;
use crate::markers;
use crate::matchers::{self, ProblemMatcher};
use crate::planner;
use crate::servers::Servers;
use crate::workflow::{
    yaml_scalar_to_string, yaml_to_json, CacheMode, Job, RunDefaults, Step, Workflow,
};
use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use serde_yaml::Value as Yaml;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_IMAGE: &str = "catthehacker/ubuntu:act-latest";
const HOST: &str = "-self-hosted";

#[derive(Clone)]
pub struct Config {
    pub repo: RepoInfo,
    pub event_name: String,
    pub event: Value,
    pub secrets: IndexMap<String, String>,
    pub vars: IndexMap<String, String>,
    pub env: IndexMap<String, String>,
    pub inputs: IndexMap<String, String>,
    /// runs-on label → image, or `-self-hosted` for the host.
    pub platforms: IndexMap<String, String>,
    pub force_host: bool,
    pub engine: Engine,
    /// The engine is remote: job directories live in volumes kept in sync with
    /// `docker cp` instead of bind mounts.
    pub copy_mode: bool,
    pub bind: bool,
    pub use_gitignore: bool,
    /// Emulate actions/checkout by copying the local repository.
    pub skip_checkout: bool,
    pub pull: bool,
    pub rebuild: bool,
    pub offline: bool,
    pub action_cache: PathBuf,
    /// Persistent tool cache (`RUNNER_TOOL_CACHE`), shared across runs like
    /// act's `act-toolcache` volume.
    pub tool_cache: PathBuf,
    /// Set by `--reuse`: fixed per-job directories and containers live here.
    pub reuse_dir: Option<PathBuf>,
    /// The repository's `.git` objects, shared by this run's checkouts.
    pub objects: Arc<actions::ObjectSnapshot>,
    /// `owner/repo@ref` (lowercase) → local directory.
    pub local_repos: IndexMap<String, PathBuf>,
    pub github: GithubHost,
    pub container_platform: Option<String>,
    /// Extra `run` options for every job container (`--container-options`, `--privileged`, ...).
    pub container_args: Vec<String>,
    /// Host path of the engine's socket, mounted into job containers (`--container-daemon-socket`).
    pub engine_socket: Option<PathBuf>,
    /// The job ids that get it (`--daemon-socket-job`); empty: every job.
    pub socket_jobs: Vec<String>,
    /// Job ids whose `GITHUB_TOKEN`, when none is given, is `SIMULATED_TOKEN`
    /// (`--simulated-token-job`).
    pub simulated_token_jobs: Vec<String>,
    pub network: Option<String>,
    /// `--matrix` filter: key → accepted values.
    pub matrix: IndexMap<String, Vec<String>>,
    /// Shared by every job of the run.
    pub shared: Arc<RunShared>,
    /// This workflow's or job's place in the run; each clone changes its own.
    pub frame: CallFrame,
    pub servers: Servers,
    pub keep: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub json: bool,
    pub log_prefix_job_id: bool,
    pub insecure_secrets: bool,
}

/// What every job of a run shares, whichever workflow (or called workflow)
/// it belongs to: one per `runthrough` invocation.
pub struct RunShared {
    /// Bounds the jobs running at once (`--concurrent-jobs`).
    pub slots: Slots,
    /// Host ports this run's jobs publish, which jobs on separate runner VMs on
    /// GitHub would not share.
    pub host_ports: HostClaims,
    /// Held by the job whose `runs-on` maps to the host (`macos-*`,
    /// `self-hosted`, `-P <label>=-self-hosted`): like a self-hosted runner, the
    /// host runs one such job at a time, since they share `~/.rustup`, the
    /// keychain and the rest of the machine (two `rustup toolchain install`s race).
    pub host_runner: HostClaims,
    /// `concurrency` groups of this run.
    pub concurrency: Concurrency,
    /// Values masked in every job's output: secrets and `::add-mask::` values.
    /// One list for the whole run, so a value masked in one job stays masked
    /// where its outputs or a called workflow print it.
    pub masks: Arc<Mutex<Vec<String>>>,
    /// Records this run for `--dashboard`; `None` with `--no-history`.
    pub history: Option<Arc<history::Recorder>>,
}

impl RunShared {
    pub fn new(concurrent_jobs: usize, history: Option<Arc<history::Recorder>>) -> RunShared {
        RunShared {
            slots: Slots::new(concurrent_jobs),
            host_ports: HostClaims::default(),
            host_runner: HostClaims::default(),
            concurrency: Concurrency::default(),
            masks: Arc::default(),
            history,
        }
    }
}

/// A workflow's or job's place in the run: the concurrency groups it holds
/// and the reusable workflow calls that led to it.
#[derive(Clone, Default)]
pub struct CallFrame {
    /// Concurrency groups (lowercased) this run of the workflow or job holds,
    /// with their tickets; a set ticket means a newer run took the group
    /// (`cancel-in-progress`).
    pub cancelled_by: Vec<(String, Arc<AtomicBool>)>,
    /// `github.workflow` inside called workflows: the top-level caller's name.
    pub caller_workflow: Option<String>,
    /// Canonical paths of the workflows calling this one, outermost first.
    pub call_chain: Vec<PathBuf>,
}

/// A placeholder `GITHUB_TOKEN` for actions that only require one to be set
/// (`anchore/sbom-action` uploading to the local artifact server). GitHub's API
/// rejects it, even for public data, so it is given only to the jobs named.
pub const SIMULATED_TOKEN: &str = "ghs_runthroughSimulatedToken0000000000";

impl Config {
    /// Whether job `id` gets the engine's socket.
    fn gets_engine_socket(&self, id: &str) -> bool {
        self.engine_socket.is_some()
            && (self.socket_jobs.is_empty() || self.socket_jobs.iter().any(|j| j == id))
    }
}

/// Counting semaphore bounding concurrently running jobs (`--concurrent-jobs`)
/// or a job's background steps. Waiters are served first come, first served by
/// the ticket they took with `queue`, so the order does not depend on which
/// thread the scheduler wakes first.
pub struct Slots {
    state: Mutex<SlotState>,
    cv: Condvar,
}

struct SlotState {
    free: usize,
    next: u64,
    queue: std::collections::VecDeque<u64>,
}

impl Slots {
    pub fn new(n: usize) -> Slots {
        Slots {
            state: Mutex::new(SlotState {
                free: n,
                next: 0,
                queue: std::collections::VecDeque::new(),
            }),
            cv: Condvar::new(),
        }
    }

    /// Take a place in line, to be redeemed with `acquire_in_turn`.
    fn queue(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        let ticket = state.next;
        state.next += 1;
        state.queue.push_back(ticket);
        ticket
    }

    /// Wait for a free slot, giving up (`None`) once `stop` returns true.
    fn acquire_unless(&self, stop: impl Fn() -> bool) -> Option<SlotGuard<'_>> {
        self.acquire_in_turn(self.queue(), stop)
    }

    /// Wait until `ticket` is first in line and a slot is free, giving up
    /// (`None`, leaving the line) once `stop` returns true.
    fn acquire_in_turn(&self, ticket: u64, stop: impl Fn() -> bool) -> Option<SlotGuard<'_>> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.free > 0 && state.queue.front() == Some(&ticket) {
                state.queue.pop_front();
                state.free -= 1;
                // The next in line may fit in another free slot.
                self.cv.notify_all();
                return Some(SlotGuard(self));
            }
            if stop() {
                state.queue.retain(|&t| t != ticket);
                self.cv.notify_all();
                return None;
            }
            state = self
                .cv
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }
}

struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().free += 1;
        self.0.cv.notify_all();
    }
}

/// Host ports a job publishes: its services' fixed host ports, or any port at
/// all when it gets the engine's socket (what it starts there is unknown).
#[derive(Debug, Clone, PartialEq)]
enum PortClaim {
    Ports(Vec<u16>),
    Any,
}

impl PortClaim {
    fn overlaps(&self, other: &PortClaim) -> bool {
        match (self, other) {
            (PortClaim::Ports(a), PortClaim::Ports(b)) => a.iter().any(|p| b.contains(p)),
            _ => true,
        }
    }
}

/// The host port of a `ports:` entry (`8080:80`, `127.0.0.1:8080:80/tcp`, a
/// range `8000-8001:8000-8001`); none for a bare container port (`80`) or an
/// empty host port, which the engine picks.
fn fixed_host_ports(entry: &str) -> Vec<u16> {
    let entry = entry.split('/').next().unwrap_or_default();
    let parts: Vec<&str> = entry.rsplitn(3, ':').collect();
    let Some(host) = parts.get(1) else {
        return vec![];
    };
    let range = |s: &str| -> Option<(u16, u16)> {
        match s.split_once('-') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => s.parse().ok().map(|p| (p, p)),
        }
    };
    range(host)
        .map(|(a, b)| (a..=b).collect())
        .unwrap_or_default()
}

/// Claims on the host that running jobs hold one at a time: the host ports
/// they publish (on GitHub each job has its own runner VM, here they share one
/// engine and the second one's `-p 5432:5432` would fail with "port is already
/// allocated"), or the whole host (`PortClaim::Any`).
#[derive(Default)]
pub struct HostClaims {
    held: Mutex<Vec<(u64, PortClaim, String)>>,
    next: AtomicU64,
    cv: Condvar,
}

impl HostClaims {
    /// Wait until no running job holds a port overlapping `claim`, calling
    /// `waiting` once with the holder's label; `None` once `stop` returns true.
    fn acquire(
        &self,
        claim: PortClaim,
        label: &str,
        waiting: impl FnOnce(&str),
        stop: impl Fn() -> bool,
    ) -> Option<HostClaimGuard<'_>> {
        let mut waiting = Some(waiting);
        let mut held = self.held.lock().unwrap();
        loop {
            match held.iter().find(|(_, c, _)| c.overlaps(&claim)) {
                None => {
                    let id = self.next.fetch_add(1, Ordering::SeqCst);
                    held.push((id, claim, label.to_string()));
                    return Some(HostClaimGuard(self, id));
                }
                Some((_, _, holder)) => {
                    if let Some(w) = waiting.take() {
                        w(holder);
                    }
                }
            }
            if stop() {
                return None;
            }
            held = self
                .cv
                .wait_timeout(held, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }
}

struct HostClaimGuard<'a>(&'a HostClaims, u64);

impl Drop for HostClaimGuard<'_> {
    fn drop(&mut self) {
        self.0
            .held
            .lock()
            .unwrap()
            .retain(|(id, _, _)| *id != self.1);
        self.0.cv.notify_all();
    }
}

/// The host ports `job` publishes (see `PortClaim`); `None` when it publishes none.
fn port_claim(id: &str, job: &Job, ctx: &Ctx, cfg: &Config) -> Option<PortClaim> {
    if cfg.gets_engine_socket(id) {
        return Some(PortClaim::Any);
    }
    let mut ports: Vec<u16> = job
        .services
        .values()
        .filter_map(|spec| expr::interpolate_json(&yaml_to_json(spec), ctx).ok())
        .filter(|spec| {
            spec.get("image")
                .is_none_or(|i| !expr::to_string(i).is_empty())
        })
        .filter_map(|spec| spec.get("ports").and_then(Value::as_array).cloned())
        .flatten()
        .flat_map(|p| fixed_host_ports(&expr::to_string(&p)))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    (!ports.is_empty()).then_some(PortClaim::Ports(ports))
}

/// `concurrency` groups within one run: one holder at a time per group (names
/// are case-insensitive). By default a newly queued run replaces the pending one;
/// `queue: max` keeps up to 100 pending in order; `cancel-in-progress` also
/// cancels the holder.
#[derive(Default)]
pub struct Concurrency {
    groups: Mutex<std::collections::HashMap<String, ConcurrencyGroup>>,
    cv: Condvar,
}

#[derive(Default)]
struct ConcurrencyGroup {
    running: Option<Arc<AtomicBool>>,
    pending: std::collections::VecDeque<Arc<AtomicBool>>,
}

const MAX_PENDING: usize = 100;

impl Concurrency {
    /// Wait until `ticket` holds `group`; `None` when it was cancelled while
    /// pending. Setting `ticket` later means the holder was cancelled.
    fn enter(
        &self,
        group: &str,
        cancel_in_progress: bool,
        queue_max: bool,
        ticket: &Arc<AtomicBool>,
    ) -> Option<ConcurrencyGuard<'_>> {
        let key = group.to_lowercase();
        let mut groups = self.groups.lock().unwrap();
        let g = groups.entry(key.clone()).or_default();
        if cancel_in_progress {
            if let Some(r) = &g.running {
                r.store(true, Ordering::SeqCst);
            }
        }
        if !queue_max {
            for p in g.pending.drain(..) {
                p.store(true, Ordering::SeqCst);
            }
        } else if g.pending.len() >= MAX_PENDING {
            return None;
        }
        g.pending.push_back(ticket.clone());
        self.cv.notify_all();
        loop {
            let g = groups.get_mut(&key).expect("group was inserted above");
            if ticket.load(Ordering::SeqCst) {
                g.pending.retain(|p| !Arc::ptr_eq(p, ticket));
                self.cv.notify_all();
                return None;
            }
            if g.running.is_none() && g.pending.front().is_some_and(|p| Arc::ptr_eq(p, ticket)) {
                g.pending.pop_front();
                g.running = Some(ticket.clone());
                return Some(ConcurrencyGuard { owner: self, key });
            }
            groups = self.cv.wait(groups).unwrap();
        }
    }
}

struct ConcurrencyGuard<'c> {
    owner: &'c Concurrency,
    key: String,
}

impl Drop for ConcurrencyGuard<'_> {
    fn drop(&mut self) {
        if let Some(g) = self.owner.groups.lock().unwrap().get_mut(&self.key) {
            g.running = None;
        }
        self.owner.cv.notify_all();
    }
}

/// `concurrency`: a group name, or `group` / `cancel-in-progress` / `queue`.
/// Returns (group, cancel-in-progress, queue: max).
fn concurrency_spec(v: &Yaml, ctx: &Ctx) -> Result<Option<(String, bool, bool)>> {
    Ok(match v {
        Yaml::Null => None,
        Yaml::Mapping(m) => {
            let group = m
                .get("group")
                .map(yaml_scalar_to_string)
                .unwrap_or_default();
            let cancel = m
                .get("cancel-in-progress")
                .map(|c| yaml_bool(c, ctx))
                .transpose()?
                .unwrap_or(false);
            let queue_max = m.get("queue").map(yaml_scalar_to_string).as_deref() == Some("max");
            Some((expr::interpolate(&group, ctx)?, cancel, queue_max))
        }
        other => Some((
            expr::interpolate(&yaml_scalar_to_string(other), ctx)?,
            false,
            false,
        )),
    }
    .filter(|(group, ..)| !group.is_empty()))
}

/// Run `body` holding a concurrency group (if `spec` names one), with `cfg`
/// extended by the ticket; `None` when cancelled while pending.
fn with_concurrency<T>(
    cfg: &Config,
    spec: Option<(String, bool, bool)>,
    log: &Logger,
    body: impl FnOnce(&Config) -> T,
) -> Option<T> {
    let Some((group, cancel_in_progress, queue_max)) = spec else {
        return Some(body(cfg));
    };
    let key = group.to_lowercase();
    // The enclosing workflow or calling job already holds it: waiting would
    // never end, so GitHub cancels.
    if cfg.frame.cancelled_by.iter().any(|(g, _)| *g == key) {
        log.line(&format!(
            "🛑 cancelled: deadlock on concurrency group '{group}', which an enclosing workflow or job holds"
        ));
        return None;
    }
    let ticket = Arc::new(AtomicBool::new(false));
    let _held = match cfg
        .shared
        .concurrency
        .enter(&group, cancel_in_progress, queue_max, &ticket)
    {
        Some(g) => g,
        None => {
            log.line(&format!(
                "🛑 cancelled: concurrency group '{group}' was taken by a newer run"
            ));
            return None;
        }
    };
    let mut cfg = cfg.clone();
    cfg.frame.cancelled_by.push((key, ticket));
    Some(body(&cfg))
}

/// How a job (or a matrix of them, or a called workflow) ended: the `result`
/// of the `needs` and `jobs` contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobResult {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl JobResult {
    pub fn as_str(self) -> &'static str {
        match self {
            JobResult::Success => "success",
            JobResult::Failure => "failure",
            JobResult::Cancelled => "cancelled",
            JobResult::Skipped => "skipped",
        }
    }
}

impl std::fmt::Display for JobResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct JobOutcome {
    pub result: JobResult,
    pub outputs: Map<String, Value>,
}

impl JobOutcome {
    fn new(result: JobResult) -> Self {
        JobOutcome {
            result,
            outputs: Map::new(),
        }
    }
}

/// Run every planned job; returns `true` when nothing failed.
pub fn run_workflow(wf: &Workflow, cfg: &Config, only: Option<&str>) -> Result<bool> {
    if cfg.event_name == "workflow_dispatch" {
        let ctx = base_ctx(wf, cfg, &json!({}), &Map::new());
        check_dispatch_inputs(wf, &ctx.contexts["inputs"])
            .with_context(|| format!("{}: invalid workflow_dispatch inputs", wf.display_name()))?;
    }
    if let Some(run_name) = &wf.run_name {
        let ctx = base_ctx(wf, cfg, &json!({}), &Map::new());
        let run_name = expr::interpolate(run_name, &ctx)?;
        if let Some(h) = &cfg.shared.history {
            h.record(json!({"wf": wf.display_name(), "run_name": run_name}));
        }
        if cfg.json {
            emit(
                &json!({"time": now_secs(), "workflow": wf.display_name(), "run_name": run_name})
                    .to_string(),
            );
        } else {
            emit(&format!("🏷️  {}: {run_name}", wf.display_name()));
        }
    }
    let spec = concurrency_spec(&wf.concurrency, &base_ctx(wf, cfg, &json!({}), &Map::new()))?;
    let log = Logger::new(&wf.display_name(), "concurrency", cfg);
    let Some(done) = with_concurrency(cfg, spec, &log, |cfg| run_jobs(wf, cfg, only, &Map::new()))
    else {
        return Ok(false);
    };
    let done = done?;
    let mut ok = true;
    for (id, out) in &done {
        let icon = match out.result {
            JobResult::Success => "✅",
            JobResult::Skipped => "⏭️ ",
            _ => {
                ok = false;
                "❌"
            }
        };
        if let Some(h) = &cfg.shared.history {
            h.record(json!({"wf": wf.display_name(), "job": id, "result": out.result}));
        }
        if cfg.json {
            emit(&json!({"time": now_secs(), "workflow": wf.display_name(), "job": id, "result": out.result}).to_string());
        } else {
            emit(&format!(
                "{icon} {} / {id}: {}",
                wf.display_name(),
                out.result
            ));
        }
    }
    Ok(ok)
}

/// Print a log line. A closed stdout (`| head`) must not abort the run before
/// containers are cleaned up, so write errors are ignored.
fn emit(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Run the jobs stage by stage; `typed_inputs` replaces the `inputs` context
/// when the workflow is called as a reusable workflow.
fn run_jobs(
    wf: &Workflow,
    cfg: &Config,
    only: Option<&str>,
    typed_inputs: &Map<String, Value>,
) -> Result<IndexMap<String, JobOutcome>> {
    let stages = planner::stages(wf, only)?;
    let mut done: IndexMap<String, JobOutcome> = IndexMap::new();
    for stage in stages {
        let results: Vec<(String, JobOutcome)> = std::thread::scope(|s| {
            let handles: Vec<_> = stage
                .iter()
                .map(|id| {
                    let done = &done;
                    s.spawn(move || run_job_matrix(wf, cfg, id, done, typed_inputs))
                })
                .collect();
            stage
                .iter()
                .zip(handles)
                .map(|(id, h)| {
                    (
                        id.clone(),
                        h.join()
                            .unwrap_or_else(|_| JobOutcome::new(JobResult::Failure)),
                    )
                })
                .collect()
        });
        done.extend(results);
    }
    Ok(done)
}

fn base_ctx(wf: &Workflow, cfg: &Config, needs: &Value, typed_inputs: &Map<String, Value>) -> Ctx {
    let mut ctx = Ctx::default();
    // Inputs are declared under the triggering event (`workflow_call` when the
    // workflow is run directly for it, otherwise `workflow_dispatch`).
    let declared_by = if cfg.event_name == "workflow_call" {
        "workflow_call"
    } else {
        "workflow_dispatch"
    };
    let mut inputs: Map<String, Value> = wf
        .input_defaults(declared_by)
        .into_iter()
        .map(|(k, v)| (k, yaml_to_json(&v)))
        .collect();
    // Precedence: workflow defaults < the event payload's `inputs` < --input.
    if let Some(Value::Object(ev)) = cfg.event.get("inputs") {
        inputs.extend(ev.clone());
    }
    for (k, v) in &cfg.inputs {
        inputs.insert(k.clone(), Value::String(v.clone()));
    }
    // Typed inputs arrive as strings from payloads and the command line.
    for (name, ty) in wf.input_types(declared_by) {
        if let Some(v) = inputs.get_mut(&name) {
            *v = coerce_input(v, &ty);
        }
    }
    if !typed_inputs.is_empty() {
        inputs = typed_inputs.clone();
    }
    let mut secrets: Map<String, Value> = cfg
        .secrets
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    secrets.entry("GITHUB_TOKEN").or_insert(json!(""));
    ctx.set("inputs", Value::Object(inputs));
    ctx.set("secrets", Value::Object(secrets));
    ctx.set("vars", json!(cfg.vars));
    ctx.set("needs", needs.clone());
    ctx.set("matrix", json!({}));
    ctx.set("strategy", json!({}));
    ctx.set("steps", json!({}));
    ctx.set("job", json!({"status": "success"}));
    ctx.set("env", json!({}));
    let mut github = cfg.repo.github_context(
        &cfg.event_name,
        &cfg.event,
        cfg.frame
            .caller_workflow
            .as_deref()
            .unwrap_or(&wf.display_name()),
        &cfg.repo.root,
    );
    github["token"] = ctx.contexts["secrets"]["GITHUB_TOKEN"].clone();
    if cfg.event_name == "workflow_dispatch" && typed_inputs.is_empty() {
        github["event"]["inputs"] = ctx.contexts["inputs"].clone();
    }
    ctx.set("github", github);
    ctx
}

/// Evaluate a `name: value` env map in order; later entries can see earlier ones.
fn eval_env(
    map: &IndexMap<String, Yaml>,
    ctx: &mut Ctx,
    into: &mut IndexMap<String, String>,
) -> Result<()> {
    for (k, v) in map {
        let val = expr::interpolate(&yaml_scalar_to_string(v), ctx)?;
        into.insert(k.clone(), val);
        ctx.set("env", json!(into));
    }
    Ok(())
}

fn yaml_bool(v: &Yaml, ctx: &Ctx) -> Result<bool> {
    Ok(match v {
        Yaml::Null => false,
        Yaml::Bool(b) => *b,
        Yaml::String(s) => expr::truthy(&expr::interpolate_value(s, ctx)?),
        other => expr::truthy(&yaml_to_json(other)),
    })
}

fn yaml_minutes(v: Option<&Yaml>, ctx: &Ctx) -> Result<Option<Duration>> {
    let Some(v) = v else { return Ok(None) };
    let n = expr::to_string(&expr::interpolate_value(&yaml_scalar_to_string(v), ctx)?);
    let mins: f64 = n
        .parse()
        .map_err(|_| anyhow!("invalid timeout-minutes '{n}'"))?;
    Ok(Some(Duration::from_secs_f64(mins * 60.0)))
}

/// The jobs `id` needs, directly or through other jobs.
fn ancestors(wf: &Workflow, id: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut todo = wf.jobs[id].needs();
    while let Some(n) = todo.pop() {
        if !found.contains(&n) {
            todo.extend(wf.jobs[&n].needs());
            found.push(n);
        }
    }
    found
}

fn run_job_matrix(
    wf: &Workflow,
    cfg: &Config,
    id: &str,
    done: &IndexMap<String, JobOutcome>,
    typed_inputs: &Map<String, Value>,
) -> JobOutcome {
    let job = &wf.jobs[id];
    let log = Logger::new(&wf.display_name(), id, cfg);
    let needs: Map<String, Value> = job
        .needs()
        .iter()
        .map(|n| {
            let o = &done[n];
            (n.clone(), json!({"result": o.result, "outputs": o.outputs}))
        })
        .collect();
    let needs = Value::Object(needs);
    let mut ctx = base_ctx(wf, cfg, &needs, typed_inputs);
    // Like GitHub, a job's status functions see every ancestor, not only its
    // direct needs; only a failed one makes `failure()` true.
    let ancestors = ancestors(wf, id);
    ctx.failed = ancestors
        .iter()
        .any(|n| done[n].result == JobResult::Failure);
    ctx.ancestor_unsucceeded = ancestors
        .iter()
        .any(|n| done[n].result != JobResult::Success);

    let result = (|| -> Result<JobOutcome> {
        let cond = job.if_cond.as_ref().map(yaml_scalar_to_string);
        if !expr::evaluate_if(cond.as_deref(), &ctx)? {
            log.line(&markers::job_skipped(" (if condition is false)"));
            return Ok(JobOutcome::new(JobResult::Skipped));
        }
        let strategy = job.strategy.as_ref();
        let matrix = match strategy {
            Some(s) => expr::interpolate_matrix(&yaml_to_json(&s.matrix), &ctx)?,
            None => Value::Null,
        };
        let mut combos = planner::expand_matrix(&matrix)?;
        if !cfg.matrix.is_empty() {
            combos.retain(|c| {
                cfg.matrix.iter().all(|(k, vals)| {
                    c.get(k)
                        .is_none_or(|v| vals.iter().any(|f| *f == expr::to_string(v)))
                })
            });
            if combos.is_empty() {
                log.line(&markers::job_skipped(
                    " (no matrix combination matches --matrix)",
                ));
                return Ok(JobOutcome::new(JobResult::Skipped));
            }
        }
        let fail_fast = match strategy.and_then(|s| s.fail_fast.as_ref()) {
            Some(v) => yaml_bool(v, &ctx)?,
            None => true,
        };
        let max_parallel = match strategy.and_then(|s| s.max_parallel.as_ref()) {
            Some(v) => expr::to_string(&expr::interpolate_value(&yaml_scalar_to_string(v), &ctx)?)
                .parse::<usize>()
                .unwrap_or(usize::MAX)
                .max(1),
            None => usize::MAX,
        };
        let total = combos.len();
        let cancel = AtomicBool::new(false);
        let outcomes: Mutex<Vec<JobOutcome>> = Mutex::new(Vec::new());
        run_legs(total, max_parallel, |index| {
            let combo = &combos[index];
            let strategy_ctx = json!({
                "fail-fast": fail_fast, "job-index": index, "job-total": total,
                "max-parallel": max_parallel.min(total),
            });
            let mut leg_ctx = ctx.clone();
            leg_ctx.set("matrix", Value::Object(combo.clone()));
            leg_ctx.set("strategy", strategy_ctx.clone());
            let mut out = match concurrency_spec(&job.concurrency, &leg_ctx) {
                Ok(spec) => with_concurrency(cfg, spec, &log, |cfg| match &job.uses {
                    Some(uses) => run_reusable(wf, cfg, id, uses, combo, ctx.clone()),
                    None => run_job(wf, cfg, id, combo, strategy_ctx, ctx.clone(), &cancel),
                })
                .unwrap_or_else(|| JobOutcome::new(JobResult::Cancelled)),
                Err(e) => {
                    log.line(&format!("❌ concurrency: {e:#}"));
                    JobOutcome::new(JobResult::Failure)
                }
            };
            // Only a failure is forgiven, per leg (`continue-on-error:
            // ${{ matrix.experimental }}`), and a forgiven one does not fail
            // fast: a skipped or cancelled leg stays so.
            if out.result == JobResult::Failure {
                match yaml_bool(&job.continue_on_error, &leg_ctx) {
                    Ok(true) => out.result = JobResult::Success,
                    Ok(false) => {}
                    Err(e) => log.line(&format!("❌ continue-on-error: {e:#}")),
                }
            }
            if out.result == JobResult::Failure && fail_fast {
                cancel.store(true, Ordering::SeqCst);
            }
            outcomes.lock().unwrap().push(out);
        });
        let outcomes = outcomes.into_inner().unwrap();
        let all_skipped =
            !outcomes.is_empty() && outcomes.iter().all(|o| o.result == JobResult::Skipped);
        let mut merged = JobOutcome::new(if all_skipped {
            JobResult::Skipped
        } else {
            JobResult::Success
        });
        for o in outcomes {
            if o.result == JobResult::Failure
                || (o.result == JobResult::Cancelled && merged.result == JobResult::Success)
            {
                merged.result = o.result;
            }
            merged.outputs.extend(o.outputs);
        }
        Ok(merged)
    })();
    result.unwrap_or_else(|e| {
        log.line(&format!("❌ {e:#}"));
        JobOutcome::new(JobResult::Failure)
    })
}

/// Run `leg(i)` for each `i` in `0..n` on its own thread, at most
/// `max_parallel` at once and in order: the next starts as soon as any ends.
fn run_legs(n: usize, max_parallel: usize, leg: impl Fn(usize) + Sync) {
    let turns = Slots::new(max_parallel.min(n).max(1));
    let tickets: Vec<u64> = (0..n).map(|_| turns.queue()).collect();
    std::thread::scope(|s| {
        for (i, ticket) in tickets.into_iter().enumerate() {
            let (turns, leg) = (&turns, &leg);
            s.spawn(move || {
                let _turn = turns.acquire_in_turn(ticket, || false);
                leg(i);
            });
        }
    });
}

fn run_job(
    wf: &Workflow,
    cfg: &Config,
    id: &str,
    combo: &Map<String, Value>,
    strategy: Value,
    mut ctx: Ctx,
    cancel: &AtomicBool,
) -> JobOutcome {
    let job = &wf.jobs[id];
    let label = if combo.is_empty() {
        id.to_string()
    } else {
        let vals: Vec<String> = combo.values().map(expr::to_string).collect();
        format!("{id} ({})", vals.join(", "))
    };
    let log = Logger::new(&wf.display_name(), &label, cfg);
    ctx.set("matrix", Value::Object(combo.clone()));
    ctx.set("strategy", strategy);
    if cfg.simulated_token_jobs.iter().any(|j| j == id)
        && ctx.contexts["secrets"]["GITHUB_TOKEN"].as_str() == Some("")
    {
        ctx.contexts["secrets"]["GITHUB_TOKEN"] = json!(SIMULATED_TOKEN);
        ctx.contexts["github"]["token"] = json!(SIMULATED_TOKEN);
    }
    // Cancelled while waiting for a slot (fail-fast, or a newer run taking its
    // concurrency group): give the group up instead of holding it.
    let cancelled = || {
        cancel.load(Ordering::SeqCst)
            || cfg
                .frame
                .cancelled_by
                .iter()
                .any(|(_, f)| f.load(Ordering::SeqCst))
    };
    // Taken before the slot, so a host job waiting here does not hold one.
    // `--host` runs every job in place of its container, in parallel as in act;
    // only jobs whose `runs-on` maps to the host stand for a runner machine.
    let on_host = !cfg.force_host
        && job.container.is_none()
        && runs_on_labels(job, &ctx)
            .and_then(|labels| resolve_platform(&labels, cfg))
            .is_ok_and(|platform| platform.is_none());
    let _host = match on_host {
        true => {
            let holder = format!("{} / {label}", wf.display_name());
            let waiting = |holder: &str| {
                log.line(&format!(
                    "⏳ waiting for {holder}: the host runs one job at a time"
                ))
            };
            let Some(guard) =
                cfg.shared
                    .host_runner
                    .acquire(PortClaim::Any, &holder, waiting, cancelled)
            else {
                log.line("🛑 cancelled while waiting for the host");
                return JobOutcome::new(JobResult::Cancelled);
            };
            Some(guard)
        }
        false => None,
    };
    let Some(_slot) = cfg.shared.slots.acquire_unless(cancelled) else {
        log.line("🛑 cancelled while waiting for a job slot");
        return JobOutcome::new(JobResult::Cancelled);
    };
    let _ports = match port_claim(id, job, &ctx, cfg) {
        Some(claim) => {
            let waiting = |holder: &str| {
                log.line(&format!(
                    "⏳ waiting for {holder} to free host ports this job publishes"
                ))
            };
            let holder = format!("{} / {label}", wf.display_name());
            let Some(guard) = cfg
                .shared
                .host_ports
                .acquire(claim, &holder, waiting, cancelled)
            else {
                log.line("🛑 cancelled while waiting for host ports");
                return JobOutcome::new(JobResult::Cancelled);
            };
            Some(guard)
        }
        None => None,
    };

    // With --reuse the job keeps a fixed directory (and container) across runs.
    let reuse_key = cfg.reuse_dir.as_ref().map(|dir| {
        let key = format!(
            "{}-{}-{:08x}",
            sanitize(&wf.display_name()),
            sanitize(&label),
            short_hash(&cfg.repo.root.to_string_lossy())
        );
        (dir.join(&key), key)
    });
    let scratch = match &reuse_key {
        Some(_) => None,
        None => match tempfile::Builder::new().prefix("runthrough-").tempdir() {
            Ok(d) => Some(d),
            Err(e) => {
                log.line(&format!("❌ creating scratch dir: {e}"));
                return JobOutcome::new(JobResult::Failure);
            }
        },
    };
    let root = match (&scratch, &reuse_key) {
        (Some(d), _) => d.path().to_path_buf(),
        (None, Some((dir, _))) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log.line(&format!("❌ creating {}: {e}", dir.display()));
                return JobOutcome::new(JobResult::Failure);
            }
            dir.clone()
        }
        (None, None) => unreachable!("either a scratch or a reuse directory exists"),
    };
    let root = root.canonicalize().unwrap_or(root);
    let reuse_name = reuse_key.map(|(_, key)| format!("runthrough-{key}"));
    let mut run = match JobRun::start(
        wf,
        cfg,
        id,
        job,
        &root,
        ctx,
        log.clone(),
        cancel,
        reuse_name,
    ) {
        Ok(r) => r,
        Err(e) if e.downcast_ref::<NoPlatform>().is_some() => {
            log.line(&markers::job_skipped(&format!(": {e}")));
            return JobOutcome::new(JobResult::Skipped);
        }
        Err(e) => {
            log.line(&format!("❌ {e:#}"));
            return JobOutcome::new(JobResult::Failure);
        }
    };
    let started = Instant::now();
    log.line(&markers::job_start(&run.backend.describe()));
    let outcome = run.execute(job);
    run.cleanup();
    let outcome = outcome.unwrap_or_else(|e| {
        log.line(&format!("❌ {e:#}"));
        JobOutcome::new(JobResult::Failure)
    });
    log.line(&markers::job_end(
        outcome.result.as_str(),
        started.elapsed().as_secs_f64(),
    ));
    match scratch {
        Some(s) if cfg.keep => {
            let kept = s.keep();
            log.line(&format!("📁 scratch kept at {}", kept.display()));
        }
        Some(_) => {}
        None => log.line(&format!("♻️  state kept at {} (--reuse)", root.display())),
    }
    outcome
}

/// Check resolved `workflow_dispatch` inputs the way GitHub does before a run
/// starts: required inputs are given, `choice` values are among the `options`,
/// and `boolean` / `number` values have that type (after `coerce_input`).
fn check_dispatch_inputs(wf: &Workflow, inputs: &Value) -> Result<()> {
    let Some(decls) = wf.section("workflow_dispatch", "inputs") else {
        return Ok(());
    };
    let mut errors = Vec::new();
    for (name, decl) in decls {
        let name = yaml_scalar_to_string(name);
        let value = inputs.get(&name).filter(|v| !v.is_null());
        let given = value.is_some_and(|v| v.as_str() != Some(""));
        let required = decl
            .get("required")
            .and_then(Yaml::as_bool)
            .unwrap_or(false);
        if required && !given {
            errors.push(format!("input '{name}' is required, but not provided"));
            continue;
        }
        let Some(value) = value.filter(|_| given) else {
            continue;
        };
        match decl.get("type").and_then(Yaml::as_str).unwrap_or("string") {
            "boolean" if !value.is_boolean() => {
                errors.push(format!(
                    "input '{name}' must be true or false, got '{}'",
                    expr::to_string(value)
                ));
            }
            "number" if !value.is_number() => {
                errors.push(format!(
                    "input '{name}' must be a number, got '{}'",
                    expr::to_string(value)
                ));
            }
            "choice" => {
                let options: Vec<String> = match decl.get("options") {
                    Some(Yaml::Sequence(seq)) => seq.iter().map(yaml_scalar_to_string).collect(),
                    _ => vec![],
                };
                let v = expr::to_string(value);
                if !options.contains(&v) {
                    errors.push(format!(
                        "input '{name}' must be one of {}, got '{v}'",
                        options.join(", ")
                    ));
                }
            }
            _ => {}
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

/// Convert a string input to its declared `boolean` / `number` type, as GitHub does.
fn coerce_input(v: &Value, ty: &str) -> Value {
    match (ty, v) {
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" | "" => Value::Bool(false),
            _ => v.clone(),
        },
        ("number", Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| v.clone()),
        _ => v.clone(),
    }
}

/// Clone the remote actions `steps` use, and those their composite actions use.
/// Errors are left for the step itself to report.
fn prefetch_actions(cfg: &Config, steps: &[Step], ctx: &Ctx, depth: usize) {
    if depth > 8 {
        return;
    }
    for (_, step) in Step::leaves(steps) {
        let Some(uses) = step.uses.as_deref() else {
            continue;
        };
        let Ok(uses) = expr::interpolate(uses, ctx) else {
            continue;
        };
        let Ok(aref) = ActionRef::parse(&uses) else {
            continue;
        };
        if aref.is_checkout() && cfg.skip_checkout {
            continue;
        }
        let ActionRef::Remote {
            owner,
            repo,
            path,
            git_ref,
        } = aref
        else {
            continue;
        };
        if cfg
            .local_repos
            .contains_key(&local_repo_key(&owner, &repo, &git_ref))
        {
            continue;
        }
        let Ok(dir) = actions::fetch_remote(
            &cfg.action_cache,
            &cfg.github,
            &owner,
            &repo,
            &git_ref,
            false,
        ) else {
            continue;
        };
        let dir = if path.is_empty() { dir } else { dir.join(path) };
        if let Ok(def) = ActionDef::load(&dir) {
            if def.runs.using == "composite" {
                prefetch_actions(cfg, &def.runs.steps, ctx, depth + 1);
            }
        }
    }
}

/// `credentials: {username, password}` of a job or service container.
fn parse_credentials(v: Option<&Value>) -> Result<Option<backend::Credentials>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let field = |k: &str| {
        v.get(k)
            .map(expr::to_string)
            .filter(|s| !s.is_empty())
            .with_context(|| format!("container credentials need a non-empty '{k}'"))
    };
    Ok(Some(backend::Credentials {
        username: field("username")?,
        password: field("password")?,
    }))
}

const CONTAINER_TOOL_CACHE: &str = "/opt/hostedtoolcache";
const TOOL_CACHE_VOLUME: &str = "runthrough-toolcache";

/// `"<ID> <VERSION_ID major>"` from an image's /etc/os-release, read in a
/// throwaway container (once per image and run) because the tool cache it
/// selects is mounted when the job container is created.
fn image_os_release(cfg: &Config, image: &str) -> Option<String> {
    static SEEN: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());
    if let Some((_, r)) = SEEN.lock().unwrap().iter().find(|(i, _)| i == image) {
        return r.clone();
    }
    let mut cmd = cfg.engine.command();
    cmd.args(["run", "--rm", "--entrypoint", "sh"]);
    cmd.args(cfg.engine.quiet_args());
    if let Some(p) = &cfg.container_platform {
        cmd.args(["--platform", p]);
    }
    cmd.args([
        image,
        "-c",
        ". /etc/os-release 2>/dev/null && echo \"$ID ${VERSION_ID%%.*}\"",
    ]);
    let r = cmd
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    SEEN.lock().unwrap().push((image.to_string(), r.clone()));
    r
}

/// Tool cache directory for a container image: tools built for one
/// distribution release (Python linked against its glibc) fail on another.
/// `"ubuntu 22"` → `linux-ubuntu22`; unknown → `linux`.
fn tool_cache_key(os_release: Option<&str>) -> String {
    let name: String = os_release
        .unwrap_or_default()
        .split_whitespace()
        .collect::<String>()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if name.is_empty() {
        "linux".into()
    } else {
        format!("linux-{name}")
    }
}

/// `"ubuntu 24"` → `ubuntu24`; other distributions → None.
fn os_release_image_os(id_and_major: &str) -> Option<String> {
    let (id, major) = id_and_major.trim().split_once(' ')?;
    (id == "ubuntu" && !major.is_empty() && major.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("ubuntu{major}"))
}

/// `ImageOS` as GitHub's images set it: `ubuntu-22.04` → `ubuntu22`,
/// `ubuntu-latest` → `ubuntu24`, `windows-2022` → `windows2022`.
fn image_os(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| l.to_ascii_lowercase())
        .find(|l| {
            ["ubuntu-", "windows-", "macos-"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(|l| match l.as_str() {
            "ubuntu-latest" => "ubuntu24".to_string(),
            _ => {
                let joined = l.replacen('-', "", 1);
                joined.split('.').next().unwrap_or(&joined).to_string()
            }
        })
        .unwrap_or_default()
}

/// `base` joined with the relative `rel`, `.` and `..` resolved lexically; `None`
/// when `rel` is absolute or climbs out of `base`.
fn path_under(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir if out != base => {
                out.pop();
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Make a job-owned directory tree writable by any user, so images that run as
/// a non-root user can write their outputs (act chowns instead). Only used on
/// the job's own scratch directories, never on the user's repository.
fn open_to_all(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        for e in walkdir::WalkDir::new(path).into_iter().flatten() {
            if e.path_is_symlink() {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                let mode = meta.permissions().mode();
                // A file linked to the run's shared git objects stays read-only:
                // a job must not change what other jobs see.
                let add = match (meta.is_dir(), meta.nlink() > 1) {
                    (true, _) => 0o777,
                    (false, true) => 0o444,
                    (false, false) => 0o666,
                };
                let _ =
                    std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(mode | add));
            }
        }
    }
}

/// Volumes standing in for the job root and the action cache in copy mode.
fn copy_volumes(base: &str, root: &Path, action_cache: &Path) -> [(String, PathBuf); 2] {
    [
        (format!("{base}-root"), root.to_path_buf()),
        (format!("{base}-actions"), action_cache.to_path_buf()),
    ]
}

/// Host directories bind-mounted at the same path into the job container and
/// into Docker action containers: the job root, the action cache, the bound
/// repository, and the directories actions substituted with
/// --local-repository run from.
fn job_mounts(cfg: &Config, root: &Path) -> Vec<PathBuf> {
    let mut mounts = vec![root.to_path_buf(), cfg.action_cache.clone()];
    if cfg.bind {
        mounts.push(cfg.repo.root.clone());
    }
    mounts.extend(cfg.local_repos.values().filter(|p| p.is_dir()).cloned());
    mounts
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn short_hash(s: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as u32
}

/// Run a reusable workflow (`jobs.<id>.uses`) as one job of the caller.
fn run_reusable(
    wf: &Workflow,
    cfg: &Config,
    id: &str,
    uses: &str,
    combo: &Map<String, Value>,
    mut ctx: Ctx,
) -> JobOutcome {
    let job = &wf.jobs[id];
    let label = if combo.is_empty() {
        id.to_string()
    } else {
        let vals: Vec<String> = combo.values().map(expr::to_string).collect();
        format!("{id} ({})", vals.join(", "))
    };
    let log = Logger::new(&wf.display_name(), &label, cfg);
    ctx.set("matrix", Value::Object(combo.clone()));
    let result = (|| -> Result<JobOutcome> {
        let uses = expr::interpolate(uses, &ctx)?;
        let path = resolve_workflow_ref(cfg, &uses)?;
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let mut chain = cfg.frame.call_chain.clone();
        chain.push(canonical(&wf.path));
        check_call_chain(&chain, &canonical(&path))?;
        log.line(&format!("📞 calling {uses}"));
        let called = Workflow::load(&path)?;
        check_call_args(&called, job, &uses)?;

        let mut inputs = Map::new();
        for (k, v) in called.input_defaults("workflow_call") {
            let v = match v {
                Yaml::String(s) => expr::interpolate_value(&s, &ctx)?,
                other => yaml_to_json(&other),
            };
            inputs.insert(k, v);
        }
        for (k, v) in &job.with {
            let v = match v {
                Yaml::String(s) => expr::interpolate_value(s, &ctx)?,
                other => expr::interpolate_json(&yaml_to_json(other), &ctx)?,
            };
            inputs.insert(k.clone(), v);
        }
        let mut nested = cfg.clone();
        nested
            .frame
            .caller_workflow
            .get_or_insert_with(|| wf.display_name());
        nested.frame.call_chain = chain;
        nested.secrets = match &job.secrets {
            Yaml::String(s) if s == "inherit" => cfg.secrets.clone(),
            Yaml::Mapping(m) => m
                .iter()
                .map(|(k, v)| {
                    Ok((
                        yaml_scalar_to_string(k),
                        expr::interpolate(&yaml_scalar_to_string(v), &ctx)?,
                    ))
                })
                .collect::<Result<_>>()?,
            _ => IndexMap::new(),
        };
        // GITHUB_TOKEN reaches every called workflow, whatever `secrets` passes.
        if let Some(token) = cfg.secrets.get("GITHUB_TOKEN") {
            nested
                .secrets
                .entry("GITHUB_TOKEN".into())
                .or_insert_with(|| token.clone());
        }
        let spec = concurrency_spec(
            &called.concurrency,
            &base_ctx(&called, &nested, &json!({}), &inputs),
        )?;
        let Some(done) = with_concurrency(&nested, spec, &log, |cfg| {
            run_jobs(&called, cfg, None, &inputs)
        }) else {
            return Ok(JobOutcome::new(JobResult::Cancelled));
        };
        let done = done?;

        let jobs: Map<String, Value> = done
            .iter()
            .map(|(k, o)| (k.clone(), json!({"result": o.result, "outputs": o.outputs})))
            .collect();
        let mut octx = ctx.clone();
        octx.set("jobs", Value::Object(jobs));
        octx.set("inputs", Value::Object(inputs));
        let mut outputs = Map::new();
        for (k, v) in called.call_outputs() {
            outputs.insert(k, Value::String(expr::interpolate(&v, &octx)?));
        }
        let failed = done
            .values()
            .any(|o| o.result == JobResult::Failure || o.result == JobResult::Cancelled);
        Ok(JobOutcome {
            result: if failed {
                JobResult::Failure
            } else {
                JobResult::Success
            },
            outputs,
        })
    })();
    result.unwrap_or_else(|e| {
        log.line(&format!("❌ {e:#}"));
        JobOutcome::new(JobResult::Failure)
    })
}

/// Check a caller's `with` and `secrets` against the called workflow's
/// `on.workflow_call` declarations, as GitHub does before starting it:
/// undeclared names and missing required ones are errors (names are
/// case-insensitive). `secrets: inherit` passes whatever exists, unchecked.
fn check_call_args(called: &Workflow, job: &Job, uses: &str) -> Result<()> {
    let has =
        |decls: &IndexMap<String, bool>, k: &str| decls.keys().any(|d| d.eq_ignore_ascii_case(k));
    let mut errors = Vec::new();
    let inputs = called.declared("workflow_call", "inputs");
    let defaults = called.input_defaults("workflow_call");
    for k in job.with.keys() {
        if !has(&inputs, k) {
            errors.push(format!("input '{k}' is not defined in the called workflow"));
        }
    }
    for (k, required) in &inputs {
        let given = job.with.keys().any(|w| w.eq_ignore_ascii_case(k));
        if *required && !given && !defaults.contains_key(k) {
            errors.push(format!("input '{k}' is required, but not provided"));
        }
    }
    if !matches!(&job.secrets, Yaml::String(s) if s == "inherit") {
        let secrets = called.declared("workflow_call", "secrets");
        let passed: Vec<String> = match &job.secrets {
            Yaml::Mapping(m) => m.keys().map(yaml_scalar_to_string).collect(),
            _ => vec![],
        };
        for k in &passed {
            if !has(&secrets, k) {
                errors.push(format!(
                    "secret '{k}' is not defined in the called workflow"
                ));
            }
        }
        for (k, required) in &secrets {
            if *required && !passed.iter().any(|p| p.eq_ignore_ascii_case(k)) {
                errors.push(format!("secret '{k}' is required, but not provided"));
            }
        }
    }
    if !errors.is_empty() {
        bail!("invalid call to {uses}:\n  {}", errors.join("\n  "));
    }
    Ok(())
}

/// GitHub's limit: the top-level caller plus nine levels of reusable workflows.
const MAX_WORKFLOW_LEVELS: usize = 10;

/// Refuse a call that would recurse, or nest deeper than GitHub allows.
/// `chain` holds the callers, outermost first; `called` is the workflow to call.
fn check_call_chain(chain: &[PathBuf], called: &Path) -> Result<()> {
    let name = |p: &Path| {
        p.file_name()
            .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into())
    };
    if chain.iter().any(|p| p == called) {
        let cycle: Vec<String> = chain
            .iter()
            .map(|p| name(p))
            .chain([name(called)])
            .collect();
        bail!(
            "reusable workflow calls itself: {} (cycles are not allowed)",
            cycle.join(" → ")
        );
    }
    if chain.len() + 1 > MAX_WORKFLOW_LEVELS {
        bail!(
            "reusable workflows nest more than {MAX_WORKFLOW_LEVELS} levels deep (calling {})",
            name(called)
        );
    }
    Ok(())
}

/// The key of `owner/repo@ref` in `Config.local_repos`.
fn local_repo_key(owner: &str, repo: &str, git_ref: &str) -> String {
    format!("{owner}/{repo}@{git_ref}").to_ascii_lowercase()
}

/// The directory of `owner/repo@ref`: the one `--local-repository` gives, or
/// the action cache's clone.
fn repository_dir(cfg: &Config, owner: &str, repo: &str, git_ref: &str) -> Result<PathBuf> {
    match cfg.local_repos.get(&local_repo_key(owner, repo, git_ref)) {
        Some(d) => Ok(d.clone()),
        None => actions::fetch_remote(
            &cfg.action_cache,
            &cfg.github,
            owner,
            repo,
            git_ref,
            cfg.offline,
        ),
    }
}

/// `./.github/workflows/x.yml` or `owner/repo/path/x.yml@ref` → local file.
fn resolve_workflow_ref(cfg: &Config, uses: &str) -> Result<PathBuf> {
    if uses.starts_with("./") {
        return Ok(cfg.repo.root.join(uses));
    }
    let (spec, git_ref) = uses
        .rsplit_once('@')
        .with_context(|| format!("reusable workflow '{uses}' is missing '@ref'"))?;
    let mut parts = spec.splitn(3, '/');
    let (Some(owner), Some(repo), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("invalid reusable workflow reference '{uses}'");
    };
    let dir = repository_dir(cfg, owner, repo, git_ref)?;
    Ok(dir.join(path))
}

// ------------------------------------------------------------------ logging

#[derive(Clone)]
struct Logger {
    prefix: String,
    workflow: String,
    job: String,
    json: bool,
    quiet: bool,
    masks: Arc<Mutex<Vec<String>>>,
    history: Option<Arc<history::Recorder>>,
}

impl Logger {
    fn new(workflow: &str, job: &str, cfg: &Config) -> Logger {
        let name = if cfg.log_prefix_job_id {
            job.to_string()
        } else {
            format!("{workflow}/{job}")
        };
        let prefix = if std::io::stdout().is_terminal() {
            let color = 31 + (name.bytes().map(u32::from).sum::<u32>() % 6);
            format!("\x1b[{color}m[{name}]\x1b[0m")
        } else {
            format!("[{name}]")
        };
        if !cfg.insecure_secrets {
            let mut masks = cfg.shared.masks.lock().unwrap();
            for s in cfg.secrets.values() {
                register_mask(&mut masks, s);
            }
        }
        Logger {
            prefix,
            workflow: workflow.to_string(),
            job: job.to_string(),
            json: cfg.json,
            quiet: cfg.quiet,
            masks: cfg.shared.masks.clone(),
            history: cfg.shared.history.clone(),
        }
    }

    fn line(&self, s: &str) {
        let masks = self.masks.lock().unwrap();
        for l in s.split('\n') {
            let l = commands::mask(l, &masks);
            if let Some(h) = &self.history {
                h.record(json!({"wf": self.workflow, "job": self.job, "msg": l}));
            }
            if self.json {
                emit(&json!({"time": now_secs(), "workflow": self.workflow, "job": self.job, "msg": l}).to_string());
            } else {
                emit(&format!("{} {l}", self.prefix));
            }
        }
    }

    /// A line a step printed; hidden with `--quiet`.
    fn output(&self, s: &str) {
        if !self.quiet {
            self.line(&format!("  | {s}"));
        }
    }

    /// Register a value to mask; one registered before (actions often set the
    /// same secret on every call) does not add to the work of every line.
    fn add_mask(&self, m: String) {
        register_mask(&mut self.masks.lock().unwrap(), &m);
    }
}

/// Add `value` and each of its lines to `masks`. Output reaches the logger one
/// line at a time, so a multi-line secret can only be matched line by line, as
/// GitHub's runner does; blank lines are not masked.
fn register_mask(masks: &mut Vec<String>, value: &str) {
    for m in std::iter::once(value).chain(value.lines()) {
        if !m.trim().is_empty() && !masks.iter().any(|x| x == m) {
            masks.push(m.to_string());
        }
    }
}

// ------------------------------------------------------------------ job state

/// Per-composite-action (or top-level) state.
struct Scope {
    /// `github.action_repository` / `github.action_ref` inside a remote composite action.
    action_ref: Option<(String, String)>,
    steps: Map<String, Value>,
    inputs: Option<Value>,
    action_path: Option<PathBuf>,
    failed: bool,
    env: IndexMap<String, String>,
    composite: bool,
}

struct PostStep {
    name: String,
    run: PostRun,
    env: IndexMap<String, String>,
    cond: Option<String>,
    /// The action's `inputs`, visible to `post-if`.
    inputs: Value,
}

enum PostRun {
    /// A command run through the job's backend (node actions).
    Argv(Vec<String>),
    /// A container run of a Docker action's `post-entrypoint`.
    Container { image: String, entrypoint: String },
}

/// What `exec_with` runs.
enum Target {
    /// A command inside the job's backend.
    Backend(Vec<String>),
    /// A sibling container started from the host, by its name; the closure
    /// builds the engine command line from the final environment.
    Container(String, BuildArgv),
}

type BuildArgv = Box<dyn Fn(&IndexMap<String, String>) -> Vec<String>>;

#[derive(Default)]
struct ExecResult {
    code: i32,
    outputs: Map<String, Value>,
    state: IndexMap<String, String>,
}

static CONTAINER_SEQ: AtomicUsize = AtomicUsize::new(0);

/// The PID plus random hex, in the names of the containers this run creates:
/// they are force-removed by name, so they must not collide with those of
/// another runthrough with the same PID (in another PID namespace) on the
/// same engine.
fn run_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        format!(
            "{}-{}",
            std::process::id(),
            &crate::servers::random_token()[..8]
        )
    })
}

/// The pid in a per-run name: `runthrough-<job>-<pid>-<8 hex>-<n>…`, where
/// `<pid>-<8 hex>` is the `run_tag` of the run that created it. Shared names
/// such as the tool cache's have none.
pub fn owner_pid(name: &str) -> Option<u32> {
    let parts: Vec<&str> = name.split('-').collect();
    parts.windows(3).find_map(|w| {
        let hex = w[1].len() == 8 && w[1].bytes().all(|b| b.is_ascii_hexdigit());
        (hex && w[2].bytes().all(|b| b.is_ascii_digit()) && !w[2].is_empty())
            .then(|| w[0].parse().ok())
            .flatten()
    })
}

/// Background steps running at once in one job; more wait for a slot.
const MAX_BACKGROUND_STEPS: usize = 10;

/// A background step running on its own thread.
struct Background<'scope> {
    id: String,
    name: String,
    continue_on_error: bool,
    /// The job's env and `GITHUB_PATH` length when it started, to tell its
    /// own additions apart.
    base_env: IndexMap<String, String>,
    base_path_len: usize,
    stop: Arc<AtomicBool>,
    handle: std::thread::ScopedJoinHandle<'scope, BackgroundDone>,
}

struct BackgroundDone {
    result: Result<(bool, Map<String, Value>)>,
    env: IndexMap<String, String>,
    path_add: Vec<String>,
    post: Vec<PostStep>,
    secs: f64,
}

struct JobRun<'a> {
    cfg: &'a Config,
    job_id: String,
    backend: Backend,
    log: Logger,
    ctx: Ctx,
    root: PathBuf,
    workspace: PathBuf,
    /// Job-level environment, including `GITHUB_ENV` additions.
    env: IndexMap<String, String>,
    path_add: Vec<String>,
    /// The backend's own PATH, read when `path_add` is first applied.
    base_path: Arc<std::sync::OnceLock<String>>,
    post: Vec<PostStep>,
    defaults: RunDefaults,
    cancel: &'a AtomicBool,
    /// Numbers `files/<seq>` and step scripts; shared with background steps.
    seq: Arc<AtomicUsize>,
    /// Set by a `cancel` step to stop this background step's process.
    stop: Option<Arc<AtomicBool>>,
    /// `ImageOS` for the job's runs-on labels.
    image_os: String,
    /// The lock file of the job's tool cache, and whether this job holds it
    /// (a composite `setup-*` action calling another must not wait on itself).
    tool_cache_lock: PathBuf,
    tool_cache_locked: bool,
    /// Job container name; also names its volumes in copy mode.
    base_name: String,
    /// Directories already copied into the job container (copy mode).
    synced: std::collections::HashSet<PathBuf>,
    /// Set while the job's pre phase runs `pre` / `pre-entrypoint` hooks.
    pre_phase: bool,
    /// The top-level step being run: its index, and the child's within a
    /// `parallel` group (None inside composite actions).
    top_step: Option<(usize, Option<usize>)>,
    /// Top-level steps whose pre hook already ran, with the state it saved.
    pre_done: std::collections::HashMap<(usize, Option<usize>), IndexMap<String, String>>,
    /// Services, network and volumes this job created, removed at its end.
    resources: JobResources,
    /// Network job, service and action containers join.
    network: Option<String>,
    /// Volumes at their own paths under the job root, mounted into the job and
    /// action containers in place of the root's bind mount: RUNNER_TEMP, and
    /// the workspace when the engine shares host files through a VM.
    job_volumes: Vec<(String, PathBuf)>,
    /// Base `job` context: container and services.
    job_ctx: Value,
    /// `ACTIONS_*` variables for the cache and artifact servers.
    service_env: Vec<(String, String)>,
    reuse: bool,
    /// Problem matchers added with `::add-matcher::`; job-wide, shared with forks.
    matchers: Arc<Mutex<Vec<ProblemMatcher>>>,
}

/// What a job creates on the engine besides its container, recorded as each
/// is created so that a failed start and the job's end remove the same set.
#[derive(Default)]
struct JobResources {
    /// Service containers: (service id, container name).
    services: Vec<(String, String)>,
    /// The network this job created.
    network_created: Option<String>,
    /// Volumes this job created (not the shared tool cache volume).
    volumes: Vec<String>,
}

impl JobResources {
    /// Remove the services, then the network and volumes unless `keep_reused`
    /// (a `--reuse` job keeps them for its next run).
    fn release(&self, engine: Engine, keep_reused: bool) {
        for (_, s) in &self.services {
            engine.remove(s);
        }
        if keep_reused {
            return;
        }
        if let Some(n) = &self.network_created {
            engine.network_remove(n);
        }
        for vol in &self.volumes {
            engine.volume_remove(vol);
        }
    }
}

/// Start `job`'s services on `network`, each recorded in `resources` once
/// started; returns the `job.services` context.
fn start_services(
    cfg: &Config,
    job: &Job,
    ctx: &Ctx,
    log: &Logger,
    base_name: &str,
    network: &Option<String>,
    resources: &mut JobResources,
) -> Result<Map<String, Value>> {
    let mut services = Map::new();
    for (svc, spec) in &job.services {
        let spec = expr::interpolate_json(&yaml_to_json(spec), ctx)?;
        let image = match &spec {
            Value::String(s) => s.clone(),
            _ => spec
                .get("image")
                .map(expr::to_string)
                .with_context(|| format!("service '{svc}' needs an image"))?,
        };
        if image.is_empty() {
            continue; // GitHub skips services whose image evaluates to ""
        }
        let list = |k: &str| -> Vec<String> {
            spec.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().map(expr::to_string).collect())
                .unwrap_or_default()
        };
        let senv = spec
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), expr::to_string(v)))
                    .collect()
            })
            .unwrap_or_default();
        let name = format!("{base_name}-{}", sanitize(svc));
        let options = spec
            .get("options")
            .map(|o| split_args(&expr::to_string(o)))
            .unwrap_or_default();
        // Apple containers have no health checks; they are polled instead.
        let (options, health) = match cfg.engine.has_health_checks() {
            true => (options, None),
            false => backend::take_health(options),
        };
        cfg.engine.remove(&name);
        log.line(&format!("🧩 service {svc}: {image}"));
        let ports = list("ports");
        check_host_ports(cfg.engine, svc, &ports)?;
        Backend::start_container(ContainerSpec {
            engine: cfg.engine,
            image: &image,
            name: name.clone(),
            mounts: vec![],
            env: senv,
            options,
            extra_args: vec![],
            extra_volumes: list("volumes"),
            ports,
            // With a host-network job, services keep a bridge network and
            // publish their ports to the host, where the job reaches them.
            network: network.clone().filter(|n| n != "host"),
            aliases: vec![svc.clone()],
            pull: cfg.pull,
            platform: cfg.container_platform.as_deref(),
            credentials: parse_credentials(spec.get("credentials"))?,
            keepalive: false,
            entrypoint: spec.get("entrypoint").map(expr::to_string),
            command: spec
                .get("command")
                .map(|c| split_args(&expr::to_string(c)))
                .unwrap_or_default(),
            reuse: false,
        })
        .inspect_err(|_| cfg.engine.remove(&name))?;
        resources.services.push((svc.clone(), name.clone()));
        cfg.engine.wait_healthy(&name, health.as_ref())?;
        services.insert(
            svc.clone(),
            json!({"id": name, "network": network.clone().unwrap_or_default(),
                   "ports": cfg.engine.ports(&name)}),
        );
    }
    Ok(services)
}

/// The fixed host ports of a `ports:` entry with their protocol (`5432/tcp`).
fn host_port_specs(entry: &str) -> Vec<String> {
    let proto = entry.split_once('/').map_or("tcp", |(_, p)| p);
    fixed_host_ports(entry)
        .into_iter()
        .map(|p| format!("{p}/{proto}"))
        .collect()
}

/// Fail before starting service `svc` when a running container outside this
/// job already publishes one of its fixed host ports (another project's
/// `docker compose`, say), naming it; the engine itself only reports "port is
/// already allocated". Jobs of this run wait for each other (`HostClaims`).
fn check_host_ports(engine: Engine, svc: &str, ports: &[String]) -> Result<()> {
    for spec in ports.iter().flat_map(|p| host_port_specs(p)) {
        if let Some(holder) = engine.port_holders(&spec).first() {
            bail!(
                "host port {spec} of service '{svc}' is already published by container \
                 {holder}; stop it (`{} stop {holder}`) or change the service's `ports`",
                engine.program()
            );
        }
    }
    Ok(())
}

/// Without network aliases (Apple containers), services are reached through
/// /etc/hosts entries in the job container.
fn add_service_hosts(backend: &Backend, services: &[(String, String)]) -> Result<()> {
    let Backend::Container(c) = backend else {
        return Ok(());
    };
    if c.engine.has_network_aliases() {
        return Ok(());
    }
    for (svc, name) in services {
        if let Some(ip) = c.engine.ipv4(name) {
            let line = format!("echo '{ip} {svc}' >> /etc/hosts");
            let argv = ["sh".to_string(), "-c".into(), line];
            backend.exec(
                &argv,
                &IndexMap::new(),
                Path::new("/"),
                None,
                None,
                &mut |_| {},
            )?;
        }
    }
    Ok(())
}

/// The persistent tool cache of host jobs, one per OS and architecture; the
/// job's own `tool/` when it cannot be created.
fn host_tool_cache(cfg: &Config, root: &Path) -> PathBuf {
    let dir = cfg.tool_cache.join(format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    if std::fs::create_dir_all(&dir).is_ok() {
        dir
    } else {
        root.join("tool")
    }
}

/// An exclusive lock on the tool cache that `path` (its lock file) guards, or
/// `None` once `cancelled`. Jobs and runthrough processes share a tool cache, and
/// a `setup-*` action installing a version deletes and copies over any install
/// of it under way: they take the lock in turn. `waiting` is called once if
/// another holds it.
fn lock_tool_cache(
    path: &Path,
    waiting: impl FnOnce(),
    cancelled: impl Fn() -> bool,
) -> std::io::Result<Option<std::fs::File>> {
    use std::fs::TryLockError;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    let mut waiting = Some(waiting);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if let Some(waiting) = waiting.take() {
            waiting();
        }
        if cancelled() {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `defaults.run`: the job's `shell` and `working-directory` over the workflow's.
fn run_defaults(wf: &Workflow, job: &Job) -> RunDefaults {
    let mut defaults = wf
        .defaults
        .as_ref()
        .and_then(|d| d.run.clone())
        .unwrap_or_default();
    if let Some(run) = job.defaults.as_ref().and_then(|d| d.run.as_ref()) {
        defaults.shell = run.shell.clone().or(defaults.shell);
        defaults.working_directory = run.working_directory.clone().or(defaults.working_directory);
    }
    defaults
}

/// The image a job runs in (`None` for the host) and its `container:` settings.
struct JobContainer {
    image: Option<String>,
    env: IndexMap<String, String>,
    options: Option<String>,
    volumes: Vec<String>,
    credentials: Option<backend::Credentials>,
}

/// Read `job.container`, or pick a platform from `runs-on` when there is none.
fn job_container(job: &Job, ctx: &Ctx, cfg: &Config) -> Result<JobContainer> {
    let (image, env, options, volumes, mut credentials) = match &job.container {
        None => (
            resolve_platform(&runs_on_labels(job, ctx)?, cfg)?,
            IndexMap::new(),
            None,
            vec![],
            None,
        ),
        Some(c) => {
            let c = expr::interpolate_json(&yaml_to_json(c), ctx)?;
            let (img, rest) = match &c {
                Value::String(s) => (s.clone(), Map::new()),
                Value::Object(m) => (
                    m.get("image")
                        .map(expr::to_string)
                        .context("job container needs an image")?,
                    m.clone(),
                ),
                _ => bail!("invalid job container"),
            };
            let cenv = rest
                .get("env")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), expr::to_string(v)))
                        .collect()
                })
                .unwrap_or_default();
            let vols = rest
                .get("volumes")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(expr::to_string).collect())
                .unwrap_or_default();
            (
                Some(img),
                cenv,
                rest.get("options").map(expr::to_string),
                vols,
                parse_credentials(rest.get("credentials"))?,
            )
        }
    };
    // act falls back to these secrets for the job container's registry.
    if credentials.is_none() {
        if let (Some(u), Some(p)) = (
            cfg.secrets.get("DOCKER_USERNAME").filter(|v| !v.is_empty()),
            cfg.secrets.get("DOCKER_PASSWORD").filter(|v| !v.is_empty()),
        ) {
            credentials = Some(backend::Credentials {
                username: u.clone(),
                password: p.clone(),
            });
        }
    }
    Ok(JobContainer {
        image,
        env,
        options,
        volumes,
        credentials,
    })
}

fn runs_on_labels(job: &Job, ctx: &Ctx) -> Result<Vec<String>> {
    let (raw, group) = match &job.runs_on {
        Yaml::Mapping(m) => (
            m.get("labels").cloned().unwrap_or(Yaml::Null),
            m.get("group").map(yaml_scalar_to_string),
        ),
        other => (other.clone(), None),
    };
    let vals = match raw {
        Yaml::String(s) => match expr::interpolate_value(&s, ctx)? {
            Value::Array(a) => a,
            v => vec![v],
        },
        Yaml::Sequence(seq) => seq
            .iter()
            .map(|v| expr::interpolate_value(&yaml_scalar_to_string(v), ctx))
            .collect::<Result<_>>()?,
        Yaml::Null => vec![],
        other => vec![yaml_to_json(&other)],
    };
    let mut labels: Vec<String> = vals.iter().map(expr::to_string).collect();
    // act matches `group` like a label.
    if let Some(g) = group {
        labels.push(expr::interpolate(&g, ctx)?);
    }
    Ok(labels)
}

/// Built-in mapping of a runs-on label: `Some(Some(image))`, `Some(None)` for the
/// host, `None` when the label is unknown.
fn default_platform(label: &str) -> Option<Option<String>> {
    let l = label.to_ascii_lowercase();
    // ubuntu-22.04 → catthehacker/ubuntu:act-22.04, as act's medium images.
    if let Some(version) = l.strip_prefix("ubuntu-").filter(|v| *v != "latest") {
        return Some(Some(format!("catthehacker/ubuntu:act-{version}")));
    }
    if l.starts_with("ubuntu") || l == "linux" {
        return Some(Some(DEFAULT_IMAGE.into()));
    }
    if l == "self-hosted" || (l.starts_with("macos") && cfg!(target_os = "macos")) {
        return Some(None);
    }
    None
}

/// Pick an image (or the host) for a job's runs-on labels.
fn resolve_platform(labels: &[String], cfg: &Config) -> Result<Option<String>> {
    if cfg.force_host {
        return Ok(None);
    }
    for label in labels {
        if let Some((_, v)) = cfg
            .platforms
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(label))
        {
            return Ok((v != HOST).then(|| v.clone()));
        }
    }
    if let Some(platform) = labels.iter().find_map(|l| default_platform(l)) {
        return Ok(platform);
    }
    Err(NoPlatform(labels.to_vec()).into())
}

/// No image or host for a job's runs-on labels; as in act, the job is skipped.
#[derive(Debug)]
struct NoPlatform(Vec<String>);

impl std::fmt::Display for NoPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no platform for runs-on {:?}; map one with -P <label>=<image> (or -P <label>={HOST} to run on the host)",
            self.0
        )
    }
}

impl std::error::Error for NoPlatform {}

impl<'a> JobRun<'a> {
    #[allow(clippy::too_many_arguments)]
    fn start(
        wf: &Workflow,
        cfg: &'a Config,
        id: &str,
        job: &Job,
        root: &Path,
        mut ctx: Ctx,
        log: Logger,
        cancel: &'a AtomicBool,
        reuse_name: Option<String>,
    ) -> Result<JobRun<'a>> {
        let workspace = if cfg.bind {
            cfg.repo.root.clone()
        } else {
            root.join("workspace")
        };
        for d in ["workspace", "temp", "tool", "files"] {
            std::fs::create_dir_all(root.join(d))?;
            open_to_all(&root.join(d));
        }
        let event_path = root.join("event.json");
        std::fs::write(&event_path, serde_json::to_vec_pretty(&cfg.event)?)?;
        ctx.workspace = Some(workspace.clone());
        ctx.contexts["github"]["workspace"] = json!(workspace.to_string_lossy());
        ctx.contexts["github"]["job"] = json!(id);
        ctx.contexts["github"]["event_path"] = json!(event_path.to_string_lossy());

        // Environment: ACT/RUNTHROUGH → workflow → job → CLI overrides. Like act,
        // `ACT` is in the env context so `if: ${{ !env.ACT }}` can skip steps.
        let mut env: IndexMap<String, String> = [("ACT", "true"), ("RUNTHROUGH", "true")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        eval_env(&wf.env, &mut ctx, &mut env)?;

        let JobContainer {
            image,
            env: cont_env,
            options,
            volumes,
            credentials,
        } = job_container(job, &ctx, cfg)?;
        let base_name = reuse_name.clone().unwrap_or_else(|| {
            format!(
                "runthrough-{}-{}-{}",
                sanitize(id),
                run_tag(),
                CONTAINER_SEQ.fetch_add(1, Ordering::SeqCst)
            )
        });
        let reuse = reuse_name.is_some();
        let mut extra_args = cfg.container_args.clone();
        let engine_socket = cfg.gets_engine_socket(id);
        if let Some(s) = cfg.engine_socket.as_ref().filter(|_| engine_socket) {
            extra_args.push("-v".into());
            extra_args.push(format!("{}:/var/run/docker.sock", s.display()));
        }
        // Lets containers reach the cache/artifact servers on the host.
        extra_args.extend(cfg.engine.host_gateway_arg().map(String::from));

        if image.is_some() || !job.services.is_empty() || engine_socket {
            cfg.engine.ensure_running()?;
        }

        // Services share a network with the job container.
        let mut network = cfg.network.clone();
        // A job without `container:` stands in for GitHub's runner VM, which reaches
        // its services, and what it starts itself through the engine's socket
        // (`docker compose up`), on ports published on localhost; as in act, it
        // shares the host network (Docker only: Apple's engine has no host network).
        if network.is_none()
            && job.container.is_none()
            && (!job.services.is_empty() || engine_socket)
            && cfg.engine.has_host_network()
        {
            network = Some("host".into());
        }
        let mut resources = JobResources::default();
        if !job.services.is_empty() && network.is_none() {
            let n = format!("{base_name}-net");
            match cfg.engine.network_create(&n) {
                Ok(()) => resources.network_created = Some(n.clone()),
                Err(e) if !reuse => return Err(e),
                Err(_) => {} // a reused job's network already exists
            }
            network = Some(n);
        }
        let started_services =
            start_services(cfg, job, &ctx, &log, &base_name, &network, &mut resources);
        let services_ctx = match started_services {
            Ok(services) => services,
            Err(e) => {
                resources.release(cfg.engine, false);
                return Err(e);
            }
        };

        // Fetch remote actions before the job container starts: a clone written
        // into the mounted action cache afterwards may not be visible inside the
        // container right away on Docker Desktop (seen as missing node_modules).
        if !cfg.offline {
            prefetch_actions(cfg, &job.steps, &ctx, 0);
        }

        // Docker Desktop's bind mounts make GNU tar fail extracting `./`
        // ("Directory renamed before its status could be extracted"), which
        // breaks the setup-* actions, so RUNNER_TEMP is a volume at its own path.
        // Through a VM's file sharing a file just written can also read back
        // empty (rustc: "memory map must have a non-zero length"), so the
        // workspace is a volume too, unless it is the bound repository or the
        // job has the engine's socket: what it binds from the workspace
        // (`docker compose`, `docker run -v $PWD:...`) the engine reads from the
        // host, which does not see a volume. Checkouts are copied into it (`sync_in`).
        let mut job_volumes = vec![];
        if image.is_some() && cfg.engine.needs_temp_volume() && !cfg.copy_mode {
            job_volumes.push((format!("{base_name}-temp"), root.join("runner-temp")));
            if cfg.engine.shares_files_through_vm() && !cfg.bind && !cfg.gets_engine_socket(id) {
                job_volumes.push((format!("{base_name}-workspace"), workspace.clone()));
            }
        }
        // The image is pulled here, before start_container, to read its OS.
        let mut os_release = None;
        if let Some(image) = &image {
            let platform = cfg.container_platform.as_deref();
            if let Err(e) = cfg
                .engine
                .ensure_image(image, cfg.pull, platform, credentials.as_ref())
            {
                resources.release(cfg.engine, false);
                return Err(e);
            }
            os_release = image_os_release(cfg, image);
        }
        let cache_key = tool_cache_key(os_release.as_deref());
        let backend = match image {
            None => Ok(Backend::Host),
            Some(image) => {
                std::fs::create_dir_all(&cfg.action_cache).ok();
                let mut mounts = vec![];
                let mut extra_volumes = volumes;
                if cfg.copy_mode {
                    for (vol, path) in copy_volumes(&base_name, root, &cfg.action_cache) {
                        cfg.engine.volume_create(&vol)?;
                        extra_volumes.push(format!("{vol}:{}", path.display()));
                        resources.volumes.push(vol);
                    }
                    let tool_volume = format!("{TOOL_CACHE_VOLUME}-{cache_key}");
                    cfg.engine.volume_create(&tool_volume)?;
                    extra_volumes.push(format!("{tool_volume}:{CONTAINER_TOOL_CACHE}"));
                } else {
                    let linux_cache = cfg.tool_cache.join(&cache_key);
                    if std::fs::create_dir_all(&linux_cache).is_ok() {
                        // Only the top level: the cache can be large.
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(
                                &linux_cache,
                                std::fs::Permissions::from_mode(0o777),
                            );
                        }
                        extra_volumes
                            .push(format!("{}:{CONTAINER_TOOL_CACHE}", linux_cache.display()));
                    }
                    for (vol, path) in &job_volumes {
                        cfg.engine.volume_create(vol)?;
                        extra_volumes.push(format!("{vol}:{}", path.display()));
                        resources.volumes.push(vol.clone());
                    }
                    mounts = job_mounts(cfg, root);
                }
                Backend::start_container(ContainerSpec {
                    engine: cfg.engine,
                    image: &image,
                    name: base_name.clone(),
                    mounts,
                    env: cont_env,
                    options: options.map(|o| split_args(&o)).unwrap_or_default(),
                    extra_args,
                    extra_volumes,
                    ports: vec![],
                    network: network.clone(),
                    // Services can reach the job container by its job ID.
                    aliases: vec![sanitize(id)],
                    pull: false, // already pulled above
                    platform: cfg.container_platform.as_deref(),
                    credentials: credentials.clone(),
                    keepalive: true,
                    entrypoint: None,
                    command: vec![],
                    reuse,
                })
            }
        };
        let backend = match backend.and_then(|b| {
            if let (true, Backend::Container(c)) = (cfg.copy_mode, &b) {
                c.engine.copy_in(&c.name, root)?;
            }
            // A new volume is root-owned; any container user must write to it.
            if let Backend::Container(c) = &b {
                for (_, path) in &job_volumes {
                    c.engine.mkdir(&c.name, path)?;
                }
            }
            Ok(b)
        }) {
            Ok(b) => b,
            Err(e) => {
                // The container goes first: a volume in use cannot be removed.
                if !job_volumes.is_empty() || cfg.copy_mode {
                    cfg.engine.remove(&base_name);
                }
                resources.release(cfg.engine, false);
                return Err(e);
            }
        };
        add_service_hosts(&backend, &resources.services)?;
        // An Ubuntu image says which release it is (act-latest follows GitHub's
        // ubuntu-latest); otherwise the runs-on label decides.
        let image_os = os_release
            .as_deref()
            .and_then(os_release_image_os)
            .unwrap_or_else(|| image_os(&runs_on_labels(job, &ctx).unwrap_or_default()));
        // Containers get the shared cache at act's path; host jobs a per-OS directory.
        let (tool_cache, tool_cache_lock) = match &backend {
            Backend::Container(_) => (
                PathBuf::from(CONTAINER_TOOL_CACHE),
                cfg.tool_cache.join(format!("{cache_key}.lock")),
            ),
            Backend::Host => {
                let dir = host_tool_cache(cfg, root);
                let lock = dir.with_extension("lock");
                (dir, lock)
            }
        };
        let (container_id, host_addr) = match &backend {
            Backend::Host => (String::new(), "127.0.0.1".to_string()),
            Backend::Container(c) => (c.name.clone(), c.engine.host_address(&c.name)),
        };
        let cache_mode = match job.cache_mode.as_ref().or(wf.cache_mode.as_ref()) {
            Some(m) => expr::interpolate(m, &ctx)?,
            None => "write".to_string(),
        };
        let Some(cache_mode) = CacheMode::parse(&cache_mode) else {
            bail!("cache-mode: expected read, write, write-only or none, got '{cache_mode}'");
        };
        let service_env = cfg.servers.job_env(&host_addr, &base_name, cache_mode);
        // `job.container` exists only for jobs with `container:` (as on GitHub).
        let job_ctx = json!({
            "status": "success",
            "container": if job.container.is_some() {
                json!({"id": container_id, "network": network.clone().unwrap_or_default()})
            } else {
                Value::Null
            },
            "services": services_ctx,
        });

        ctx.set(
            "runner",
            json!({
                "os": backend.runner_os(), "arch": backend.runner_arch(), "name": "runthrough",
                "temp": if job_volumes.is_empty() { root.join("temp") } else { root.join("runner-temp") }.to_string_lossy(),
                "tool_cache": tool_cache.to_string_lossy(),
                "environment": "self-hosted", "debug": if cfg.verbose { "1" } else { "" },
            }),
        );
        ctx.set("job", job_ctx.clone());
        eval_env(&job.env, &mut ctx, &mut env)?;
        for (k, v) in &cfg.env {
            env.insert(k.clone(), v.clone());
        }
        ctx.set("env", json!(env));

        Ok(JobRun {
            cfg,
            job_id: id.to_string(),
            backend,
            log,
            ctx,
            root: root.to_path_buf(),
            workspace,
            env,
            path_add: vec![],
            base_path: Arc::default(),
            post: vec![],
            defaults: run_defaults(wf, job),
            cancel,
            seq: Arc::new(AtomicUsize::new(0)),
            stop: None,
            resources,
            network,
            job_volumes,
            job_ctx,
            service_env,
            reuse,
            image_os,
            tool_cache_lock,
            tool_cache_locked: false,
            base_name,
            synced: std::collections::HashSet::new(),
            pre_phase: false,
            top_step: None,
            pre_done: std::collections::HashMap::new(),
            matchers: Arc::default(),
        })
    }

    fn next_seq(&self) -> usize {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The job container's name when the engine is remote (copy mode).
    fn copy_target(&self) -> Option<(Engine, String)> {
        match &self.backend {
            Backend::Container(c) if self.cfg.copy_mode => Some((c.engine, c.name.clone())),
            _ => None,
        }
    }

    /// Whether `path` lies on one of the job's volumes, which the host does not see.
    fn on_job_volume(&self, path: &Path) -> bool {
        self.job_volumes.iter().any(|(_, p)| path.starts_with(p))
    }

    /// A directory below a volume's root (the roots are made at the start).
    fn below_job_volume(&self, path: &Path) -> bool {
        self.job_volumes
            .iter()
            .any(|(_, p)| path.starts_with(p) && path != p)
    }

    /// Copy a host directory into the job container (copy mode, or a directory
    /// on the job's volumes such as a checkout into the workspace); `once` skips
    /// directories already sent, such as cached actions.
    fn sync_in(&mut self, dir: &Path, once: bool) -> Result<()> {
        let target = match &self.backend {
            Backend::Container(c) if self.on_job_volume(dir) => Some((c.engine, c.name.clone())),
            _ => self.copy_target(),
        };
        let Some((engine, name)) = target else {
            return Ok(());
        };
        if once && !self.synced.insert(dir.to_path_buf()) {
            return Ok(());
        }
        engine.copy_in(&name, dir)
    }

    /// Send `temp/` to the job container (copy mode): all of it the first time,
    /// then only the files written since, such as the next step's script.
    fn sync_temp(&mut self, engine: Engine, name: &str) -> Result<()> {
        let temp = self.root.join("temp");
        let first = self.synced.insert(temp.clone());
        let new: Vec<PathBuf> = std::fs::read_dir(&temp)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && !self.synced.contains(p))
            .collect();
        if first {
            engine.copy_in(name, &temp)?;
        } else {
            for f in &new {
                engine.copy_file_in(name, f)?;
            }
        }
        self.synced.extend(new);
        Ok(())
    }

    /// Stop the job container (unless reused), its services, network and volumes.
    fn cleanup(&self) {
        self.backend.stop();
        self.resources.release(self.cfg.engine, self.reuse);
    }

    fn execute(&mut self, job: &Job) -> Result<JobOutcome> {
        let mut scope = Scope {
            action_ref: None,
            steps: Map::new(),
            inputs: None,
            action_path: None,
            failed: false,
            env: IndexMap::new(),
            composite: false,
        };
        let timeout = yaml_minutes(job.timeout_minutes.as_ref(), &self.ctx)?;
        let deadline = timeout.map(|t| Instant::now() + t);
        // There are no deployments or protection rules locally; the environment
        // is only reported, with its `url` evaluated once the steps ran.
        let (environment, env_url) = match &job.environment {
            Yaml::Null => (None, None),
            Yaml::Mapping(m) => (
                m.get("name").map(yaml_scalar_to_string),
                m.get("url").map(yaml_scalar_to_string),
            ),
            other => (Some(yaml_scalar_to_string(other)), None),
        };
        if let Some(name) = &environment {
            let name = expr::interpolate(name, &self.ctx)?;
            self.log.line(&format!("🌍 environment {name}"));
        }
        // Like GitHub and act, every action's pre hook runs before the first step.
        self.run_pre_phase(&job.steps, &mut scope)?;
        std::thread::scope(|threads| {
            self.run_job_steps(&job.steps, &mut scope, deadline, threads)
        })?;

        // Post steps run in reverse registration order.
        while let Some(p) = self.post.pop() {
            let mut ctx = self.ctx_for(&scope);
            ctx.failed = scope.failed;
            ctx.set("inputs", p.inputs.clone());
            if !expr::evaluate_if(Some(p.cond.as_deref().unwrap_or("always()")), &ctx)? {
                continue;
            }
            self.log
                .line(&markers::step_start(&format!("Post {}", p.name)));
            let cwd = self.workspace.clone();
            let r = match &p.run {
                PostRun::Argv(argv) => self.exec(argv, &p.env, &cwd, None)?,
                PostRun::Container { image, entrypoint } => {
                    self.run_container_step(image, Some(entrypoint), &[], &p.env, None)?
                }
            };
            if r.code != 0 {
                self.log
                    .line(&format!("❌ post step failed (exit {})", r.code));
                scope.failed = true;
            }
        }

        let ctx = self.ctx_for(&scope);
        if let Some(url) = env_url {
            let url = expr::interpolate(&url, &ctx)?;
            if !url.is_empty() {
                self.log.line(&format!("🔗 environment url {url}"));
            }
        }
        let mut outputs = Map::new();
        for (k, v) in &job.outputs {
            outputs.insert(k.clone(), Value::String(expr::interpolate(v, &ctx)?));
        }
        let result = if self.cancelled() && !scope.failed {
            JobResult::Cancelled
        } else if scope.failed {
            JobResult::Failure
        } else {
            JobResult::Success
        };
        Ok(JobOutcome { result, outputs })
    }

    /// Expression context as seen from inside `scope`.
    fn ctx_for(&self, scope: &Scope) -> Ctx {
        let mut ctx = self.ctx.clone();
        ctx.set("steps", Value::Object(scope.steps.clone()));
        if let Some(inputs) = &scope.inputs {
            ctx.set("inputs", inputs.clone());
        }
        if let Some(p) = &scope.action_path {
            ctx.contexts["github"]["action_path"] = json!(p.to_string_lossy());
        }
        if let Some((repo, git_ref)) = &scope.action_ref {
            ctx.contexts["github"]["action_repository"] = json!(repo);
            ctx.contexts["github"]["action_ref"] = json!(git_ref);
        }
        let mut env = self.env.clone();
        env.extend(scope.env.clone());
        ctx.set("env", json!(env));
        let mut job = self.job_ctx.clone();
        job["status"] = json!(if scope.failed { "failure" } else { "success" });
        ctx.set("job", job);
        ctx.failed = scope.failed;
        ctx.ancestor_unsucceeded = false;
        ctx.cancelled = self.cancelled();
        ctx
    }

    /// Cancelled by fail-fast, a timeout, or a newer run taking a concurrency group.
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
            || self
                .cfg
                .frame
                .cancelled_by
                .iter()
                .any(|(_, f)| f.load(Ordering::SeqCst))
    }

    /// Run the `pre` hooks of the job's top-level actions, in step order.
    fn run_pre_phase(&mut self, steps: &[Step], scope: &mut Scope) -> Result<()> {
        for (key, step) in Step::leaves(steps) {
            let Some(uses) = &step.uses else { continue };
            // Only remote actions exist before the job runs; local ones (`./…`)
            // appear after checkout and run their pre hook in place.
            if uses.starts_with("./") || uses.starts_with("../") || uses.starts_with("docker://") {
                continue;
            }
            let mut ctx = self.ctx_for(scope);
            let cond = step.if_cond.as_ref().map(yaml_scalar_to_string);
            if !expr::evaluate_if(cond.as_deref(), &ctx).unwrap_or(false) {
                continue;
            }
            let step_id = step.id.clone().unwrap_or_else(|| Step::default_id(key));
            self.pre_phase = true;
            self.top_step = Some(key);
            let r = self.run_step(step, &step_id, scope, &mut ctx, None);
            self.pre_phase = false;
            match r {
                Ok((true, _)) => {}
                Ok((false, _)) => scope.failed = true,
                Err(e) => {
                    self.log.line(&format!("❌ Pre {uses}: {e:#}"));
                    scope.failed = true;
                }
            }
        }
        Ok(())
    }

    /// A composite action's steps.
    fn run_steps(
        &mut self,
        steps: &[Step],
        scope: &mut Scope,
        deadline: Option<Instant>,
    ) -> Result<()> {
        for (i, step) in steps.iter().enumerate() {
            self.run_listed_step(i, step, scope, deadline);
        }
        Ok(())
    }

    /// A job's steps, including `background` steps run on their own threads and
    /// the `wait`, `wait-all`, `cancel` and `parallel` steps that control them.
    fn run_job_steps<'scope, 'env>(
        &mut self,
        steps: &'env [Step],
        scope: &mut Scope,
        deadline: Option<Instant>,
        threads: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<()>
    where
        'a: 'scope,
    {
        let mut running: Vec<Background<'scope>> = Vec::new();
        let limit = Arc::new(Slots::new(MAX_BACKGROUND_STEPS));
        for (i, step) in steps.iter().enumerate() {
            let step_id = step
                .id
                .clone()
                .unwrap_or_else(|| Step::default_id((i, None)));
            if step.wait.is_some() || step.wait_all.is_some() {
                let ids = step.wait.is_some().then(|| step.wait_ids());
                let label = match &ids {
                    Some(ids) => format!("Wait for {}", ids.join(", ")),
                    None => "Wait for all background steps".to_string(),
                };
                let ctx = self.ctx_for(scope);
                let name = step
                    .name
                    .as_ref()
                    .map_or(label, |_| self.step_name(step, &ctx));
                self.log.line(&markers::step_start(&name));
                let ok = self.join_background(&mut running, ids.as_deref(), scope);
                let continue_on_error = yaml_bool(&step.continue_on_error, &ctx).unwrap_or(false);
                self.record_control_step(step_id, &name, ok, continue_on_error, scope);
            } else if let Some(target) = &step.cancel {
                let ctx = self.ctx_for(scope);
                let name = step.name.as_ref().map_or_else(
                    || format!("Cancel {target}"),
                    |_| self.step_name(step, &ctx),
                );
                self.log.line(&markers::step_start(&name));
                let found = running.iter().position(|b| &b.id == target);
                let ok = match found {
                    Some(at) => {
                        let bg = running.remove(at);
                        bg.stop.store(true, Ordering::SeqCst);
                        self.finish_background(bg, scope, true);
                        true
                    }
                    None if scope.steps.contains_key(target) => true,
                    None => {
                        self.log
                            .line(&format!("  ❌ no background step '{target}'"));
                        false
                    }
                };
                self.record_control_step(step_id, &name, ok, false, scope);
            } else if let Some(group) = &step.parallel {
                let ctx = self.ctx_for(scope);
                let name = step.name.as_ref().map_or_else(
                    || "Run steps in parallel".to_string(),
                    |_| self.step_name(step, &ctx),
                );
                self.log.line(&markers::step_start(&name));
                let mut ids = Vec::new();
                for (j, child) in group.iter().enumerate() {
                    let id = child
                        .id
                        .clone()
                        .unwrap_or_else(|| Step::default_id((i, Some(j))));
                    ids.push(id.clone());
                    self.top_step = Some((i, Some(j)));
                    self.spawn_background(
                        child,
                        id,
                        scope,
                        deadline,
                        threads,
                        &mut running,
                        &limit,
                    );
                }
                let ok = self.join_background(&mut running, Some(&ids), scope);
                let continue_on_error = yaml_bool(&step.continue_on_error, &ctx).unwrap_or(false);
                self.record_control_step(step_id, &name, ok, continue_on_error, scope);
            } else {
                match yaml_bool(&step.background, &self.ctx_for(scope)) {
                    Ok(true) => {
                        self.top_step = Some((i, None));
                        self.spawn_background(
                            step,
                            step_id,
                            scope,
                            deadline,
                            threads,
                            &mut running,
                            &limit,
                        );
                    }
                    Ok(false) => self.run_listed_step(i, step, scope, deadline),
                    Err(e) => {
                        self.log.line(&format!("❌ background: {e:#}"));
                        scope.failed = true;
                    }
                }
            }
        }
        // An implicit wait-all before the post steps; a cancelled job stops its
        // background steps, which end cancelled.
        if self.cancelled() {
            for bg in &running {
                bg.stop.store(true, Ordering::SeqCst);
            }
            for bg in running.drain(..) {
                self.finish_background(bg, scope, true);
            }
        }
        if !running.is_empty() && !self.join_background(&mut running, None, scope) {
            scope.failed = true;
        }
        Ok(())
    }

    /// Start a background step on its own thread with a copy of the job's state.
    #[allow(clippy::too_many_arguments)]
    fn spawn_background<'scope, 'env>(
        &mut self,
        step: &'env Step,
        step_id: String,
        scope: &mut Scope,
        deadline: Option<Instant>,
        threads: &'scope std::thread::Scope<'scope, 'env>,
        running: &mut Vec<Background<'scope>>,
        limit: &Arc<Slots>,
    ) where
        'a: 'scope,
    {
        let ctx = self.ctx_for(scope);
        let name = self.step_name(step, &ctx);
        let cond = step.if_cond.as_ref().map(yaml_scalar_to_string);
        match expr::evaluate_if(cond.as_deref(), &ctx) {
            Ok(true) => {}
            Ok(false) => {
                if self.cfg.verbose {
                    self.log.line(&markers::step_skipped(&name));
                }
                scope
                    .steps
                    .insert(step_id, step_entry(Map::new(), "skipped", "skipped"));
                return;
            }
            Err(e) => {
                self.log.line(&format!("❌ {name}: {e:#}"));
                scope.failed = true;
                return;
            }
        }
        self.log
            .line(&markers::step_start(&format!("{name} (background)")));
        let continue_on_error = yaml_bool(&step.continue_on_error, &ctx).unwrap_or(false);
        let stop = Arc::new(AtomicBool::new(false));
        let mut fork = self.fork(stop.clone());
        let mut fork_scope = Scope {
            action_ref: None,
            steps: scope.steps.clone(),
            inputs: None,
            action_path: None,
            failed: scope.failed,
            env: IndexMap::new(),
            composite: false,
        };
        // Take a place in line here, so background steps get slots in step order.
        let ticket = limit.queue();
        let (limit, id, stopped) = (limit.clone(), step_id.clone(), stop.clone());
        let handle = threads.spawn(move || {
            let started = Instant::now();
            // Cancelled while waiting for a slot: it never runs.
            let result = match limit.acquire_in_turn(ticket, || stopped.load(Ordering::SeqCst)) {
                Some(_slot) => {
                    let mut ctx = fork.ctx_for(&fork_scope);
                    fork.run_step(step, &id, &mut fork_scope, &mut ctx, deadline)
                }
                None => Ok((false, Map::new())),
            };
            BackgroundDone {
                result,
                env: fork.env,
                path_add: fork.path_add,
                post: fork.post,
                secs: started.elapsed().as_secs_f64(),
            }
        });
        running.push(Background {
            id: step_id,
            name,
            continue_on_error,
            base_env: self.env.clone(),
            base_path_len: self.path_add.len(),
            stop,
            handle,
        });
    }

    /// Wait for the background steps listed in `ids` (all with `None`); false
    /// when one of them failed or an ID names no background step.
    fn join_background(
        &mut self,
        running: &mut Vec<Background<'_>>,
        ids: Option<&[String]>,
        scope: &mut Scope,
    ) -> bool {
        let mut ok = true;
        if let Some(ids) = ids {
            for id in ids {
                let known = running.iter().any(|b| &b.id == id) || scope.steps.contains_key(id);
                if !known {
                    self.log.line(&format!("  ❌ no background step '{id}'"));
                    ok = false;
                }
            }
        }
        let (waited, rest): (Vec<_>, Vec<_>) = std::mem::take(running)
            .into_iter()
            .partition(|b| ids.is_none_or(|ids| ids.contains(&b.id)));
        *running = rest;
        for bg in waited {
            ok &= self.finish_background(bg, scope, false);
        }
        ok
    }

    /// Join one background step and apply what it changed: its `steps` entry,
    /// `GITHUB_ENV` / `GITHUB_PATH` additions, and post steps. Returns false
    /// when it failed (and `continue-on-error` is not set).
    fn finish_background(
        &mut self,
        bg: Background<'_>,
        scope: &mut Scope,
        mut cancelled: bool,
    ) -> bool {
        // A job cancelled while waiting stops the step instead of waiting it out.
        while !bg.handle.is_finished() {
            if !cancelled && self.cancelled() {
                bg.stop.store(true, Ordering::SeqCst);
                cancelled = true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let done = bg.handle.join().unwrap_or_else(|_| BackgroundDone {
            result: Err(anyhow!("background step panicked")),
            env: bg.base_env.clone(),
            path_add: vec![],
            post: vec![],
            secs: 0.0,
        });
        for (k, v) in done.env {
            if bg.base_env.get(&k) != Some(&v) {
                self.env.insert(k, v);
            }
        }
        let added = done.path_add.len().saturating_sub(bg.base_path_len);
        self.path_add
            .splice(0..0, done.path_add.into_iter().take(added));
        self.post.extend(done.post);
        let (outcome, outputs) = match done.result {
            Ok((true, out)) => ("success", out),
            _ if cancelled => ("cancelled", Map::new()),
            Ok((false, out)) => ("failure", out),
            Err(e) => {
                self.log.line(&format!("❌ {}: {e:#}", bg.name));
                ("failure", Map::new())
            }
        };
        let conclusion = self.conclude(&bg.name, outcome, bg.continue_on_error, Some(done.secs));
        scope
            .steps
            .insert(bg.id, step_entry(outputs, outcome, conclusion));
        conclusion != "failure"
    }

    /// Result of a `wait`, `wait-all`, `cancel` or `parallel` step.
    fn record_control_step(
        &mut self,
        step_id: String,
        name: &str,
        ok: bool,
        continue_on_error: bool,
        scope: &mut Scope,
    ) {
        let outcome = if ok { "success" } else { "failure" };
        let conclusion = self.conclude(name, outcome, continue_on_error, None);
        if conclusion == "failure" {
            scope.failed = true;
        }
        scope
            .steps
            .insert(step_id, step_entry(Map::new(), outcome, conclusion));
    }

    /// Log how a step ended and return its conclusion: a failure under
    /// `continue-on-error` concludes as a success.
    fn conclude(
        &self,
        name: &str,
        outcome: &'static str,
        continue_on_error: bool,
        secs: Option<f64>,
    ) -> &'static str {
        let conclusion = if outcome == "failure" && continue_on_error {
            "success"
        } else {
            outcome
        };
        self.log
            .line(&markers::step_end(name, outcome, conclusion, secs));
        conclusion
    }

    /// A copy of the job's state for a background step; changes come back
    /// through `finish_background`.
    fn fork(&self, stop: Arc<AtomicBool>) -> JobRun<'a> {
        JobRun {
            cfg: self.cfg,
            job_id: self.job_id.clone(),
            backend: self.backend.clone(),
            log: self.log.clone(),
            ctx: self.ctx.clone(),
            root: self.root.clone(),
            workspace: self.workspace.clone(),
            env: self.env.clone(),
            path_add: self.path_add.clone(),
            base_path: self.base_path.clone(),
            post: vec![],
            defaults: self.defaults.clone(),
            cancel: self.cancel,
            seq: self.seq.clone(),
            stop: Some(stop),
            image_os: self.image_os.clone(),
            tool_cache_lock: self.tool_cache_lock.clone(),
            // A background step runs beside its parent: it takes the lock itself.
            tool_cache_locked: false,
            base_name: self.base_name.clone(),
            synced: self.synced.clone(),
            pre_phase: false,
            top_step: self.top_step,
            pre_done: self.pre_done.clone(),
            // Created and removed by the job, not by its background steps.
            resources: JobResources::default(),
            network: self.network.clone(),
            job_volumes: self.job_volumes.clone(),
            job_ctx: self.job_ctx.clone(),
            service_env: self.service_env.clone(),
            reuse: self.reuse,
            matchers: self.matchers.clone(),
        }
    }

    /// Run one listed step: its `if`, logging, and its `steps` context entry.
    fn run_listed_step(
        &mut self,
        i: usize,
        step: &Step,
        scope: &mut Scope,
        deadline: Option<Instant>,
    ) {
        self.top_step = (!scope.composite).then_some((i, None));
        let step_id = step
            .id
            .clone()
            .unwrap_or_else(|| Step::default_id((i, None)));
        let mut ctx = self.ctx_for(scope);
        let name = self.step_name(step, &ctx);
        let cond = step.if_cond.as_ref().map(yaml_scalar_to_string);
        if deadline.is_some_and(|d| Instant::now() > d) {
            self.log.line("❌ job timed out");
            self.cancel.store(true, Ordering::SeqCst);
            ctx.cancelled = true;
        }
        let run_it = match expr::evaluate_if(cond.as_deref(), &ctx) {
            Ok(b) => b,
            Err(e) => {
                self.log.line(&format!("❌ {name}: {e:#}"));
                scope.failed = true;
                return;
            }
        };
        if !run_it {
            if self.cfg.verbose {
                self.log.line(&markers::step_skipped(&name));
            }
            scope
                .steps
                .insert(step_id, step_entry(Map::new(), "skipped", "skipped"));
            return;
        }
        self.log.line(&markers::step_start(&name));
        let started = Instant::now();
        let result = if scope.composite && step.is_control() {
            Err(anyhow!(
                "background, wait, wait-all, cancel and parallel are not allowed in composite actions"
            ))
        } else {
            self.run_step(step, &step_id, scope, &mut ctx, deadline)
        };
        let (outcome, outputs) = match result {
            Ok((true, out)) => ("success", out),
            Ok((false, out)) => ("failure", out),
            Err(e) => {
                self.log.line(&format!("❌ {e:#}"));
                ("failure", Map::new())
            }
        };
        let continue_on_error = yaml_bool(&step.continue_on_error, &ctx).unwrap_or(false);
        let secs = started.elapsed().as_secs_f64();
        let conclusion = self.conclude(&name, outcome, continue_on_error, Some(secs));
        if conclusion == "failure" {
            scope.failed = true;
        }
        scope
            .steps
            .insert(step_id, step_entry(outputs, outcome, conclusion));
    }

    fn step_name(&self, step: &Step, ctx: &Ctx) -> String {
        if let Some(n) = &step.name {
            return expr::interpolate(n, ctx).unwrap_or_else(|_| n.clone());
        }
        if let Some(r) = &step.run {
            return format!(
                "Run {}",
                r.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            );
        }
        step.uses
            .as_deref()
            .map(|u| format!("Run {u}"))
            .unwrap_or_else(|| "step".into())
    }

    /// Returns (succeeded, outputs).
    fn run_step(
        &mut self,
        step: &Step,
        step_id: &str,
        scope: &mut Scope,
        ctx: &mut Ctx,
        deadline: Option<Instant>,
    ) -> Result<(bool, Map<String, Value>)> {
        // The env context keeps the job's values (including GITHUB_ENV additions)
        // while step `env:` is evaluated; only the step's own values are returned.
        let mut env = scope.env.clone();
        let mut visible: IndexMap<String, String> = ctx.contexts["env"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), expr::to_string(v)))
                    .collect()
            })
            .unwrap_or_default();
        for (k, v) in &step.env {
            let val = expr::interpolate(&yaml_scalar_to_string(v), ctx)?;
            env.insert(k.clone(), val.clone());
            visible.insert(k.clone(), val);
            ctx.set("env", json!(visible));
        }
        let mut timeout = yaml_minutes(step.timeout_minutes.as_ref(), ctx)?;
        if let Some(d) = deadline {
            let left = d.saturating_duration_since(Instant::now());
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        env.insert(
            "GITHUB_ACTION".into(),
            step_id.trim_start_matches("__").to_string(),
        );

        match (&step.run, &step.uses) {
            (Some(script), None) => self.run_script(step, script, scope, ctx, env, timeout),
            (None, Some(uses)) => self.run_action(step, uses, scope, ctx, env, timeout, deadline),
            _ => bail!("a step must have exactly one of `run` or `uses`"),
        }
    }

    fn resolve_dir(&self, wd: Option<&str>, ctx: &Ctx) -> Result<PathBuf> {
        Ok(match wd {
            Some(w) => self.workspace.join(expr::interpolate(w, ctx)?),
            None => self.workspace.clone(),
        })
    }

    fn run_script(
        &mut self,
        step: &Step,
        script: &str,
        scope: &Scope,
        ctx: &Ctx,
        env: IndexMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<(bool, Map<String, Value>)> {
        let shell = match (&step.shell, scope.composite) {
            (Some(s), _) => Some(expr::interpolate(s, ctx)?),
            (None, true) => bail!("composite action steps with `run` must set `shell`"),
            (None, false) => self
                .defaults
                .shell
                .as_deref()
                .map(|s| expr::interpolate(s, ctx))
                .transpose()?,
        };
        // A Windows host defaults to PowerShell, as GitHub's Windows runners do.
        let shell = shell.or_else(|| {
            (cfg!(windows) && matches!(self.backend, Backend::Host)).then(|| {
                if on_path("pwsh") {
                    "pwsh"
                } else {
                    "powershell"
                }
                .to_string()
            })
        });
        let wd = step.working_directory.clone().or_else(|| {
            (!scope.composite)
                .then(|| self.defaults.working_directory.clone())
                .flatten()
        });
        let cwd = self.resolve_dir(wd.as_deref(), ctx)?;
        let body = wrap_script(shell.as_deref(), &expr::interpolate(script, ctx)?);
        let script_path = self.root.join("temp").join(format!(
            "step-{}.{}",
            self.next_seq(),
            shell_ext(shell.as_deref())
        ));
        std::fs::write(&script_path, body)?;
        let argv = shell_argv(shell.as_deref(), &script_path);
        if self.cfg.verbose {
            self.log.line(&format!("  $ {}", argv.join(" ")));
        }
        let r = self.exec(&argv, &env, &cwd, timeout)?;
        if r.code != 0 {
            self.log.line(&format!("  exit code {}", r.code));
            if r.code == 127 {
                self.log
                    .line("  (127: command not found — is it installed in the image?)");
            }
        }
        Ok((r.code == 0, r.outputs))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_action(
        &mut self,
        step: &Step,
        uses: &str,
        scope: &mut Scope,
        ctx: &Ctx,
        env: IndexMap<String, String>,
        timeout: Option<Duration>,
        deadline: Option<Instant>,
    ) -> Result<(bool, Map<String, Value>)> {
        let installs = !self.pre_phase
            && !self.tool_cache_locked
            && ActionRef::parse(&expr::interpolate(uses, ctx)?)?.installs_tools();
        if !installs {
            return self.run_action_unlocked(step, uses, scope, ctx, env, timeout, deadline);
        }
        let waiting = || {
            self.log
                .line("  ⏳ waiting for another job installing into the tool cache")
        };
        let lock = match lock_tool_cache(&self.tool_cache_lock, waiting, || self.cancelled()) {
            Ok(lock) => lock,
            Err(e) => {
                self.log.line(&format!(
                    "  running without the tool cache lock {}: {e}",
                    self.tool_cache_lock.display()
                ));
                None
            }
        };
        self.tool_cache_locked = lock.is_some();
        let result = self.run_action_unlocked(step, uses, scope, ctx, env, timeout, deadline);
        self.tool_cache_locked = false;
        drop(lock);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_action_unlocked(
        &mut self,
        step: &Step,
        uses: &str,
        scope: &mut Scope,
        ctx: &Ctx,
        mut env: IndexMap<String, String>,
        timeout: Option<Duration>,
        deadline: Option<Instant>,
    ) -> Result<(bool, Map<String, Value>)> {
        let uses = expr::interpolate(uses, ctx)?;
        let aref = ActionRef::parse(&uses)?;
        let with: IndexMap<String, String> = step
            .with
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    expr::interpolate(&yaml_scalar_to_string(v), ctx)?,
                ))
            })
            .collect::<Result<_>>()?;

        let pre_only = self.pre_phase;
        if pre_only
            && (matches!(aref, ActionRef::Docker(_))
                || (aref.is_checkout() && self.cfg.skip_checkout))
        {
            return Ok((true, Map::new()));
        }
        if aref.is_checkout() && self.cfg.skip_checkout {
            return self.checkout(&with).map(|_| (true, Map::new()));
        }
        let dir = match &aref {
            ActionRef::Local(p) => self.workspace.join(p),
            ActionRef::Remote {
                owner,
                repo,
                path,
                git_ref,
            } => {
                let d = repository_dir(self.cfg, owner, repo, git_ref)?;
                if path.is_empty() {
                    d
                } else {
                    d.join(path)
                }
            }
            ActionRef::Docker(img) => {
                // `with.args` / `with.entrypoint` configure the container; the rest are inputs.
                for (k, v) in &with {
                    if !k.eq_ignore_ascii_case("args") && !k.eq_ignore_ascii_case("entrypoint") {
                        env.insert(actions::input_env_name(k), v.clone());
                    }
                }
                let args = with_value(&with, "args")
                    .map(|a| split_args(&a))
                    .unwrap_or_default();
                self.cfg.engine.ensure_image(
                    img,
                    self.cfg.pull,
                    self.cfg.container_platform.as_deref(),
                    None,
                )?;
                let r = self.run_container_step(
                    img,
                    with_value(&with, "entrypoint").as_deref(),
                    &args,
                    &env,
                    timeout,
                )?;
                return Ok((r.code == 0, r.outputs));
            }
        };
        let def = ActionDef::load(&dir)?;

        let mut inputs = Map::new();
        for (name, input) in &def.inputs {
            let val = match with_value(&with, name) {
                Some(v) => v,
                None => match &input.default {
                    Some(d) => expr::interpolate(&yaml_scalar_to_string(d), ctx)?,
                    None => {
                        if !pre_only
                            && input
                                .required
                                .as_ref()
                                .is_some_and(|r| yaml_bool(r, ctx).unwrap_or(false))
                        {
                            self.log
                                .line(&format!("  ⚠️  missing required input '{name}'"));
                        }
                        String::new()
                    }
                },
            };
            inputs.insert(name.clone(), Value::String(val));
        }
        // `with.args` / `with.entrypoint` configure a Docker action's container.
        let docker_override = |k: &str| {
            def.runs.using == "docker"
                && (k.eq_ignore_ascii_case("args") || k.eq_ignore_ascii_case("entrypoint"))
        };
        for (k, v) in &with {
            if docker_override(k) {
                continue;
            }
            if !inputs.keys().any(|n| n.eq_ignore_ascii_case(k)) {
                if !pre_only {
                    self.log.line(&format!("  ⚠️  unexpected input '{k}'"));
                }
                inputs.insert(k.clone(), Value::String(v.clone()));
            }
        }
        env.insert(
            "GITHUB_ACTION_PATH".into(),
            dir.to_string_lossy().into_owned(),
        );
        env.insert(
            "GITHUB_ACTION_REPOSITORY".into(),
            match &aref {
                ActionRef::Remote { owner, repo, .. } => format!("{owner}/{repo}"),
                _ => String::new(),
            },
        );
        env.insert(
            "GITHUB_ACTION_REF".into(),
            match &aref {
                ActionRef::Remote { git_ref, .. } => git_ref.clone(),
                _ => String::new(),
            },
        );
        let using = def.runs.using.as_str();
        if def.is_node() {
            for (k, v) in &inputs {
                env.insert(actions::input_env_name(k), expr::to_string(v));
            }
            self.sync_in(&dir, true)?;
            let node_argv = |file: &str| {
                vec![
                    "node".to_string(),
                    dir.join(file).to_string_lossy().into_owned(),
                ]
            };
            let cwd = self.workspace.clone();
            let failed = scope.failed;
            let pre_if = || {
                let mut c = ctx.clone();
                c.failed = failed;
                c.set("inputs", Value::Object(inputs.clone()));
                expr::evaluate_if(Some(def.runs.pre_if.as_deref().unwrap_or("always()")), &c)
            };
            let pre = def.runs.pre.as_deref().map(|pre| {
                let (cwd, node_argv) = (&cwd, &node_argv);
                move |this: &mut Self, env: &IndexMap<String, String>| {
                    this.exec(&node_argv(pre), env, cwd, timeout)
                }
            });
            if let Some(ended) = self.pre_hook(step, ctx, &mut env, pre_if, pre)? {
                return Ok(ended);
            }
            let main = def
                .runs
                .main
                .as_deref()
                .context("node action is missing runs.main")?;
            let r = self.exec(&node_argv(main), &env, &cwd, timeout)?;
            if let Some(post) = &def.runs.post {
                let run = PostRun::Argv(node_argv(post));
                self.push_post(step, ctx, run, &env, &r.state, &def, &inputs);
            }
            Ok((r.code == 0, r.outputs))
        } else if using == "composite" && pre_only {
            Ok((true, Map::new()))
        } else if using == "composite" {
            let mut inner = Scope {
                action_ref: match &aref {
                    ActionRef::Remote {
                        owner,
                        repo,
                        git_ref,
                        ..
                    } => Some((format!("{owner}/{repo}"), git_ref.clone())),
                    _ => None,
                },
                steps: Map::new(),
                inputs: Some(Value::Object(inputs)),
                action_path: Some(dir.clone()),
                failed: false,
                env,
                composite: true,
            };
            self.run_steps(&def.runs.steps, &mut inner, deadline)?;
            let ictx = self.ctx_for(&inner);
            let mut outputs = Map::new();
            for (k, o) in &def.outputs {
                if let Some(v) = &o.value {
                    outputs.insert(k.clone(), Value::String(expr::interpolate(v, &ictx)?));
                }
            }
            Ok((!inner.failed, outputs))
        } else if using == "docker" {
            let image_ref = def
                .runs
                .image
                .as_deref()
                .context("docker action is missing runs.image")?;
            let engine = self.cfg.engine;
            let image = match image_ref.strip_prefix("docker://") {
                Some(img) => {
                    engine.ensure_image(
                        img,
                        self.cfg.pull,
                        self.cfg.container_platform.as_deref(),
                        None,
                    )?;
                    img.to_string()
                }
                None => {
                    let dockerfile = dir.join(image_ref);
                    // Stable across runs (the workspace path is not), so the
                    // image is rebuilt only with --rebuild.
                    let tag = format!(
                        "runthrough-action-{:08x}",
                        short_hash(&format!(
                            "{}|{uses}|{image_ref}",
                            self.cfg.repo.root.display()
                        ))
                    );
                    // As in act, the build context is the Dockerfile's directory.
                    let context = dockerfile.parent().unwrap_or(&dir).to_path_buf();
                    engine.build_image(&context, &dockerfile, &tag, self.cfg.rebuild)?;
                    tag
                }
            };
            let mut ictx = ctx.clone();
            ictx.set("inputs", Value::Object(inputs.clone()));
            // As on GitHub, `with.args` / `with.entrypoint` replace the action's own.
            let args: Vec<String> = match with_value(&with, "args") {
                Some(a) => split_args(&a),
                None => def
                    .runs
                    .args
                    .iter()
                    .map(|a| expr::interpolate(&yaml_scalar_to_string(a), &ictx))
                    .collect::<Result<_>>()?,
            };
            let entrypoint =
                with_value(&with, "entrypoint").or_else(|| def.runs.entrypoint.clone());
            for (k, v) in &def.runs.env {
                env.insert(
                    k.clone(),
                    expr::interpolate(&yaml_scalar_to_string(v), &ictx)?,
                );
            }
            for (k, v) in &inputs {
                env.insert(actions::input_env_name(k), expr::to_string(v));
            }
            // Unlike a node action's `pre`, `pre-entrypoint` runs without `pre-if`.
            let pre = def.runs.pre_entrypoint.as_deref().map(|pre| {
                let image = &image;
                move |this: &mut Self, env: &IndexMap<String, String>| {
                    this.run_container_step(image, Some(pre), &[], env, timeout)
                }
            });
            if let Some(ended) = self.pre_hook(step, ctx, &mut env, || Ok(true), pre)? {
                return Ok(ended);
            }
            let r = self.run_container_step(&image, entrypoint.as_deref(), &args, &env, timeout)?;
            if let Some(post) = &def.runs.post_entrypoint {
                let run = PostRun::Container {
                    image: image.clone(),
                    entrypoint: post.clone(),
                };
                self.push_post(step, ctx, run, &env, &r.state, &def, &inputs);
            }
            Ok((r.code == 0, r.outputs))
        } else {
            bail!("action runtime `using: {using}` is not supported ({uses})")
        }
    }

    /// An action's pre hook, run by `run` if `cond` holds, unless the job's
    /// pre phase already ran it: its saved state goes into `env` as `STATE_*`.
    /// `Some` when the step ends here: the hook failed, or only the pre phase runs.
    fn pre_hook(
        &mut self,
        step: &Step,
        ctx: &Ctx,
        env: &mut IndexMap<String, String>,
        cond: impl FnOnce() -> Result<bool>,
        run: Option<impl FnOnce(&mut Self, &IndexMap<String, String>) -> Result<ExecResult>>,
    ) -> Result<Option<(bool, Map<String, Value>)>> {
        let pre_only = self.pre_phase;
        let pre_key = self.top_step;
        let earlier = pre_key
            .filter(|_| !pre_only)
            .and_then(|k| self.pre_done.remove(&k));
        if let Some(state) = earlier {
            env.extend(state);
        } else if let Some(run) = run {
            let mut state = IndexMap::new();
            if cond()? {
                self.log.line(&markers::step_start(&format!(
                    "Pre {}",
                    self.step_name(step, ctx)
                )));
                let r = run(self, env)?;
                if r.code != 0 {
                    return Ok(Some((false, r.outputs)));
                }
                // State saved by `pre` is visible to `main` and `post`.
                for (k, v) in &r.state {
                    state.insert(format!("STATE_{k}"), v.clone());
                }
            }
            if pre_only {
                if let Some(k) = pre_key {
                    self.pre_done.insert(k, state);
                }
                return Ok(Some((true, Map::new())));
            }
            env.extend(state);
        }
        if pre_only {
            return Ok(Some((true, Map::new())));
        }
        Ok(None)
    }

    /// Register an action's post hook with the state its main step saved.
    #[allow(clippy::too_many_arguments)]
    fn push_post(
        &mut self,
        step: &Step,
        ctx: &Ctx,
        run: PostRun,
        env: &IndexMap<String, String>,
        state: &IndexMap<String, String>,
        def: &ActionDef,
        inputs: &Map<String, Value>,
    ) {
        let mut env = env.clone();
        for (k, v) in state {
            env.insert(format!("STATE_{k}"), v.clone());
        }
        self.post.push(PostStep {
            name: self.step_name(step, ctx),
            run,
            env,
            cond: def.runs.post_if.clone(),
            inputs: Value::Object(inputs.clone()),
        });
    }

    /// Run `image` as a sibling container sharing the job's directories and network.
    fn run_container_step(
        &mut self,
        image: &str,
        entrypoint: Option<&str>,
        args: &[String],
        env: &IndexMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<ExecResult> {
        // `entrypoint: /bin/sh -c` → `--entrypoint /bin/sh` and `-c` before the args.
        let mut ep_words = entrypoint.map(split_args).unwrap_or_default();
        let entrypoint = (!ep_words.is_empty()).then(|| ep_words.remove(0));
        let entrypoint = entrypoint.as_deref();
        let args: Vec<String> = ep_words.into_iter().chain(args.iter().cloned()).collect();
        let args = args.as_slice();
        let engine = self.cfg.engine;
        let name = format!(
            "runthrough-step-{}-{}",
            run_tag(),
            CONTAINER_SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let container = name.clone();
        let workspace = self.workspace.to_string_lossy().into_owned();
        // Host paths, or in copy mode the job's volumes, at the same paths.
        let volumes: Vec<String> = if self.cfg.copy_mode {
            if matches!(self.backend, Backend::Host) {
                bail!("docker actions in a job without a container need a local engine (DOCKER_HOST is remote)");
            }
            copy_volumes(&self.base_name, &self.root, &self.cfg.action_cache)
                .into_iter()
                .map(|(v, p)| format!("{v}:{}", p.display()))
                .collect()
        } else {
            job_mounts(self.cfg, &self.root)
                .iter()
                .map(|m| format!("{0}:{0}", m.to_string_lossy()))
                .chain(
                    self.job_volumes
                        .iter()
                        .map(|(v, p)| format!("{v}:{}", p.display())),
                )
                .collect()
        };
        let network = self.network.clone();
        let platform = self.cfg.container_platform.clone();
        let entrypoint = entrypoint.map(String::from);
        let image = image.to_string();
        let args = args.to_vec();
        let build = move |env: &IndexMap<String, String>| {
            let mut a: Vec<String> = vec![
                engine.program().into(),
                "run".into(),
                "--rm".into(),
                "--name".into(),
                name.clone(),
                "-w".into(),
                workspace.clone(),
            ];
            for v in &volumes {
                a.extend(["-v".into(), v.clone()]);
            }
            a.extend(engine.quiet_args().iter().map(|s| s.to_string()));
            if let Some(n) = &network {
                a.extend(["--network".into(), n.clone()]);
            }
            if let Some(p) = &platform {
                a.extend(["--platform".into(), p.clone()]);
            }
            a.extend(engine.host_gateway_arg().map(String::from));
            if let Some(ep) = &entrypoint {
                a.extend(["--entrypoint".into(), ep.clone()]);
            }
            for (k, v) in env {
                a.extend(["-e".into(), format!("{k}={v}")]);
            }
            a.push(image.clone());
            a.extend(args.iter().cloned());
            a
        };
        let cwd = self.workspace.clone();
        self.exec_with(
            env,
            &cwd,
            timeout,
            Target::Container(container, Box::new(build)),
        )
    }

    fn checkout(&mut self, with: &IndexMap<String, String>) -> Result<()> {
        let given = |k: &str| with.get(k).filter(|v| !v.is_empty());
        // The destination is deleted and written on the host: like
        // actions/checkout, refuse a `path` that leaves the workspace.
        let dest = match given("path") {
            Some(p) => path_under(&self.workspace, p).with_context(|| {
                format!(
                    "checkout path '{p}' is not under the workspace {}",
                    self.workspace.display()
                )
            })?,
            None => self.workspace.clone(),
        };
        if let Some(repo) =
            given("repository").filter(|r| !r.eq_ignore_ascii_case(&self.cfg.repo.repository))
        {
            // With --bind the workspace is the user's own repository: never delete in it.
            if self.cfg.bind && dest.exists() {
                bail!(
                    "checking out {repo} into {} would overwrite files in the bound repository; set `path` to a new directory",
                    dest.display()
                );
            }
            let (owner, name) = repo
                .split_once('/')
                .with_context(|| format!("repository must be owner/name, got '{repo}'"))?;
            let (url, _) = self.cfg.github.source(owner, name);
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            actions::clone_at(
                &url,
                given("ref").map(String::as_str),
                &dest,
                given("token").map(String::as_str),
            )?;
            if !self.cfg.bind {
                open_to_all(&dest);
            }
            self.log
                .line(&format!("  cloned {repo} → {}", dest.display()));
            return self.sync_in(&dest, false);
        }
        if self.cfg.bind {
            self.log
                .line("  (bind mode: workspace is the repository itself)");
            return Ok(());
        }
        actions::copy_repo(
            &self.cfg.repo.root,
            &dest,
            self.cfg.use_gitignore,
            &self.cfg.objects,
        )?;
        // As in GitHub's checkout of a pushed branch, `origin/<branch>` is the
        // commit being run, not the local copy's last fetch (`dorny/paths-filter`
        // diffs `before` against it). Left alone when that commit is not in the
        // repository (no commits yet, or an event's `after` from elsewhere).
        // (Without `.git`, git would find an enclosing repository instead.)
        if let Some(branch) = self
            .cfg
            .repo
            .git_ref
            .strip_prefix("refs/heads/")
            .filter(|_| dest.join(".git").exists())
        {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&dest)
                .args(["update-ref", &format!("refs/remotes/origin/{branch}")])
                .arg(&self.cfg.repo.sha)
                .stderr(std::process::Stdio::null())
                .status();
        }
        open_to_all(&dest);
        self.log.line(&format!(
            "  copied {} → {}",
            self.cfg.repo.root.display(),
            dest.display()
        ));
        self.sync_in(&dest, false)
    }

    /// Run a process with runner env + environment files, handling workflow commands.
    fn exec(
        &mut self,
        argv: &[String],
        step_env: &IndexMap<String, String>,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ExecResult> {
        self.exec_with(step_env, cwd, timeout, Target::Backend(argv.to_vec()))
    }

    fn exec_with(
        &mut self,
        step_env: &IndexMap<String, String>,
        cwd: &Path,
        timeout: Option<Duration>,
        target: Target,
    ) -> Result<ExecResult> {
        let files = self.root.join("files").join(self.next_seq().to_string());
        std::fs::create_dir_all(&files)?;
        let file = |n: &str| files.join(n);
        for n in ["output", "env", "path", "summary", "state"] {
            std::fs::write(file(n), "")?;
        }
        open_to_all(&files);
        // In a container the working directory may lie outside the shared paths
        // (e.g. `/root`), so it is created there rather than on the host.
        match &self.backend {
            Backend::Container(c)
                if !self.cfg.copy_mode
                    && (!cwd.starts_with(&self.root) || self.below_job_volume(cwd)) =>
            {
                c.engine.mkdir(&c.name, cwd)?
            }
            Backend::Container(_) if self.cfg.copy_mode => {}
            _ => std::fs::create_dir_all(cwd)
                .with_context(|| format!("creating working directory {}", cwd.display()))?,
        }

        let mut env = self.runner_env();
        env.extend(self.env.clone());
        env.extend(step_env.clone());
        for (k, n) in [
            ("GITHUB_OUTPUT", "output"),
            ("GITHUB_ENV", "env"),
            ("GITHUB_PATH", "path"),
            ("GITHUB_STEP_SUMMARY", "summary"),
            ("GITHUB_STATE", "state"),
        ] {
            env.insert(k.into(), file(n).to_string_lossy().into_owned());
        }
        // GITHUB_PATH additions apply to commands in the job's backend; a
        // sibling container keeps its image's PATH. PATH is set directly: a
        // `sh -c` wrapper would drop names dash cannot export (INPUT_GO-VERSION).
        if matches!(target, Target::Backend(_)) && !self.path_add.is_empty() {
            let base = match env.get("PATH") {
                Some(p) => p.clone(),
                None => self.base_path.get_or_init(|| self.backend.path()).clone(),
            };
            env.insert("PATH".into(), format!("{}:{base}", self.path_add.join(":")));
        }

        let mut res = ExecResult::default();
        let mut env_adds: IndexMap<String, String> = IndexMap::new();
        let mut path_adds: Vec<String> = Vec::new();
        let mut stop_token: Option<String> = None;
        // Whether a client in the step failed to reach the engine's socket.
        let mut socket_unreachable = false;
        // This step's copy, so background steps never interleave a multi-line
        // match; additions and removals also go to the job's list.
        let mut matchers = self.matchers.lock().unwrap().clone();
        matchers.iter_mut().for_each(ProblemMatcher::restart);
        let job_matchers = self.matchers.clone();
        let workspace = self.workspace.clone();
        let log = self.log.clone();
        let verbose = self.cfg.verbose;
        // As on GitHub, the deprecated set-env/add-path commands need an opt-in.
        let unsecure = env
            .get("ACTIONS_ALLOW_UNSECURE_COMMANDS")
            .is_some_and(|v| v == "true");
        let on_line = &mut |line: &str| {
            if let Some(tok) = &stop_token {
                if line.trim() == format!("::{tok}::") {
                    stop_token = None;
                } else {
                    log.output(line);
                }
                return;
            }
            match commands::parse_command(line) {
                Some(Command::SetOutput(k, v)) => {
                    res.outputs.insert(k, Value::String(v));
                }
                Some(Command::SaveState(k, v)) => {
                    res.state.insert(k, v);
                }
                Some(Command::SetEnv(k, v)) if unsecure => {
                    env_adds.insert(k, v);
                }
                Some(Command::AddPath(p)) if unsecure => path_adds.push(p),
                Some(Command::SetEnv(..) | Command::AddPath(_)) => log.line(
                    "  ❗ the set-env and add-path commands are disabled; set ACTIONS_ALLOW_UNSECURE_COMMANDS=true or use GITHUB_ENV / GITHUB_PATH",
                ),
                Some(Command::AddMask(m)) => log.add_mask(m),
                Some(Command::Group(g)) => log.line(&format!("  ▼ {g}")),
                Some(Command::EndGroup) => {}
                Some(Command::Debug(d)) => {
                    if verbose {
                        log.line(&format!("  🐛 {d}"));
                    }
                }
                Some(Command::Annotation { level, message }) => {
                    let icon = match level.as_str() {
                        "error" => "❗",
                        "warning" => "⚠️ ",
                        _ => "ℹ️ ",
                    };
                    log.line(&format!("  {icon} {message}"));
                }
                Some(Command::StopCommands(t)) => stop_token = Some(t),
                Some(Command::AddMatcher(path)) => {
                    let path = workspace.join(path.trim());
                    match std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))
                        .and_then(|json| matchers::parse(&json))
                    {
                        Ok(new) => {
                            let mut job = job_matchers.lock().unwrap();
                            matchers::register(&mut job, new.clone());
                            matchers::register(&mut matchers, new);
                        }
                        Err(e) => log.line(&format!("  ⚠️  add-matcher: {e:#}")),
                    }
                }
                Some(Command::RemoveMatcher(owner)) => {
                    matchers::remove(&mut job_matchers.lock().unwrap(), &owner);
                    matchers::remove(&mut matchers, &owner);
                }
                None => {
                    log.output(line);
                    socket_unreachable |= line.contains("unix:///var/run/docker.sock")
                        && line.to_ascii_lowercase().contains("connect");
                    for m in &mut matchers {
                        if let Some(issue) = m.feed(line) {
                            let icon = match issue.severity.as_str() {
                                "error" => "❗",
                                "warning" => "⚠️ ",
                                _ => "ℹ️ ",
                            };
                            log.line(&format!("  {icon} {}{}", issue.location(), issue.message));
                        }
                    }
                }
            }
        };
        if let Some((engine, name)) = self.copy_target() {
            self.sync_temp(engine, &name)?;
            engine.copy_in(&name, &files)?;
            // Only create the working directory: copying it would overwrite
            // changes earlier steps made inside the container.
            engine.mkdir(&name, cwd)?;
        }
        let code = match &target {
            Target::Backend(argv) => {
                self.backend
                    .exec(argv, &env, cwd, timeout, self.stop.as_deref(), on_line)?
            }
            Target::Container(name, build) => {
                let code = Backend::Host.exec(
                    &build(&env),
                    &IndexMap::new(),
                    cwd,
                    timeout,
                    self.stop.as_deref(),
                    on_line,
                );
                // A timeout or `cancel` kills only the `docker run` client, and
                // `--rm` does not stop the container: remove it before reading
                // the files it could still write to.
                self.cfg.engine.remove(name);
                code?
            }
        };
        res.code = code;
        if let Some((engine, name)) = self.copy_target() {
            engine.copy_out(&name, &files)?;
        }
        if code != 0
            && socket_unreachable
            && matches!(self.backend, Backend::Container(_))
            && !self.cfg.gets_engine_socket(&self.job_id)
        {
            self.log.line(&format!(
                "  (the job container has no engine socket: pass --daemon-socket-job {} to mount /var/run/docker.sock)",
                self.job_id
            ));
        }

        for (k, v) in commands::parse_env_file(&std::fs::read_to_string(file("output"))?)? {
            res.outputs.insert(k, Value::String(v));
        }
        res.state
            .extend(commands::parse_env_file(&std::fs::read_to_string(file(
                "state",
            ))?)?);
        env_adds.extend(commands::parse_env_file(&std::fs::read_to_string(file(
            "env",
        ))?)?);
        for l in std::fs::read_to_string(file("path"))?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            path_adds.push(l.trim().to_string());
        }
        self.env.extend(env_adds);
        for p in path_adds {
            self.path_add.insert(0, p);
        }
        let summary = std::fs::read_to_string(file("summary"))?;
        if !summary.trim().is_empty() {
            self.log.line("  📝 step summary:");
            for l in summary.lines() {
                self.log.line(&format!("  │ {l}"));
            }
        }
        Ok(res)
    }

    fn runner_env(&self) -> IndexMap<String, String> {
        let g = &self.ctx.contexts["github"];
        let s = |k: &str| expr::to_string(&g[k]);
        let r = &self.ctx.contexts["runner"];
        let mut e = IndexMap::new();
        for (k, v) in [
            ("CI", "true".to_string()),
            ("GITHUB_ACTIONS", "true".into()),
            ("RUNTHROUGH", "true".into()),
            ("ACT", "true".into()),
            ("GITHUB_WORKFLOW", s("workflow")),
            ("GITHUB_RUN_ID", s("run_id")),
            ("GITHUB_RUN_NUMBER", s("run_number")),
            ("GITHUB_RUN_ATTEMPT", s("run_attempt")),
            ("GITHUB_JOB", self.job_id.clone()),
            ("GITHUB_ACTOR", s("actor")),
            ("GITHUB_TRIGGERING_ACTOR", s("triggering_actor")),
            ("GITHUB_REPOSITORY", s("repository")),
            ("GITHUB_REPOSITORY_OWNER", s("repository_owner")),
            ("GITHUB_EVENT_NAME", s("event_name")),
            ("GITHUB_EVENT_PATH", s("event_path")),
            ("GITHUB_WORKSPACE", s("workspace")),
            ("GITHUB_SHA", s("sha")),
            ("GITHUB_REF", s("ref")),
            ("GITHUB_REF_NAME", s("ref_name")),
            ("GITHUB_REF_TYPE", s("ref_type")),
            ("GITHUB_HEAD_REF", s("head_ref")),
            ("GITHUB_BASE_REF", s("base_ref")),
            ("GITHUB_SERVER_URL", s("server_url")),
            ("GITHUB_API_URL", s("api_url")),
            ("GITHUB_GRAPHQL_URL", s("graphql_url")),
            ("RUNNER_OS", expr::to_string(&r["os"])),
            ("RUNNER_ARCH", expr::to_string(&r["arch"])),
            ("RUNNER_NAME", "runthrough".into()),
            ("RUNNER_TEMP", expr::to_string(&r["temp"])),
            ("RUNNER_TOOL_CACHE", expr::to_string(&r["tool_cache"])),
            ("RUNNER_PERFLOG", "/dev/null".into()),
            ("RUNNER_TRACKING_ID", String::new()),
            ("GITHUB_RETENTION_DAYS", s("retention_days")),
            ("ImageOS", self.image_os.clone()),
        ] {
            e.insert(k.to_string(), v);
        }
        if matches!(self.backend, Backend::Container(_)) {
            e.insert("LANG".into(), "C.UTF-8".into());
            // GitHub's workspace lives under HOME (/home/runner/work/…), which
            // actions rely on, e.g. gitleaks uploads results.sarif with HOME as
            // the artifact root; the job root plays /home/runner here.
            if !self.cfg.bind {
                e.insert("HOME".into(), self.root.to_string_lossy().into_owned());
            }
        }
        e.extend(self.service_env.iter().cloned());
        e
    }
}

/// The `with:` value for `name`; input names are case-insensitive.
fn with_value(with: &IndexMap<String, String>, name: &str) -> Option<String> {
    with.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// A step's entry in the `steps` context.
fn step_entry(outputs: Map<String, Value>, outcome: &str, conclusion: &str) -> Value {
    json!({"outputs": outputs, "outcome": outcome, "conclusion": conclusion})
}

fn shell_ext(shell: Option<&str>) -> &'static str {
    match shell.map(str::trim) {
        Some("python") => "py",
        Some("pwsh" | "powershell") => "ps1",
        Some("cmd") => "cmd",
        Some("node") => "js",
        _ => "sh",
    }
}

/// What GitHub's runner (and act) add around a script: PowerShell stops on the
/// first error and returns the last native exit code; cmd turns echo off.
fn wrap_script(shell: Option<&str>, body: &str) -> String {
    match shell.map(str::trim) {
        Some("pwsh" | "powershell") => format!(
            "$ErrorActionPreference = 'stop'\n{body}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}\n"
        ),
        Some("cmd") => format!("@echo off\n{body}"),
        _ => body.to_string(),
    }
}

/// Whether `program` is found on PATH.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|d| d.join(program).is_file() || d.join(format!("{program}.exe")).is_file())
    })
}

/// Command line for a `shell:` value; `{0}` is replaced by the script path.
fn shell_argv(shell: Option<&str>, script: &Path) -> Vec<String> {
    let p = script.to_string_lossy().into_owned();
    let v = |parts: &[&str]| {
        parts
            .iter()
            .map(|s| s.replace("{0}", &p))
            .collect::<Vec<_>>()
    };
    match shell.map(str::trim) {
        // Unspecified: bash if present, else sh (as on GitHub-hosted Linux runners).
        None => v(&["sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash -e \"$0\"; else exec sh -e \"$0\"; fi", "{0}"]),
        Some("bash") => v(&["bash", "--noprofile", "--norc", "-eo", "pipefail", "{0}"]),
        Some("sh") => v(&["sh", "-e", "{0}"]),
        Some("python") => v(&["python3", "{0}"]),
        Some("pwsh") => v(&["pwsh", "-command", ". '{0}'"]),
        Some("powershell") => v(&["powershell", "-command", ". '{0}'"]),
        Some("node") => v(&["node", "{0}"]),
        Some("cmd") => v(&["cmd", "/D", "/E:ON", "/V:OFF", "/S", "/C", "CALL \"{0}\""]),
        Some(custom) => {
            let mut parts: Vec<String> = custom.split_whitespace().map(|s| s.replace("{0}", &p)).collect();
            if !custom.contains("{0}") {
                parts.push(p);
            }
            parts
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_host_ports_come_from_mappings_only() {
        assert_eq!(fixed_host_ports("5432:5432"), vec![5432]);
        assert_eq!(fixed_host_ports("127.0.0.1:8080:80/tcp"), vec![8080]);
        assert_eq!(
            fixed_host_ports("8000-8002:9000-9002"),
            vec![8000, 8001, 8002]
        );
        assert!(fixed_host_ports("5432").is_empty());
        assert!(fixed_host_ports("127.0.0.1::80").is_empty());
    }

    #[test]
    fn host_port_specs_keep_the_protocol() {
        assert_eq!(host_port_specs("5432:5432"), vec!["5432/tcp"]);
        assert_eq!(host_port_specs("127.0.0.1:53:53/udp"), vec!["53/udp"]);
        assert_eq!(
            host_port_specs("8000-8001:9000-9001/tcp"),
            vec!["8000/tcp", "8001/tcp"]
        );
        assert!(host_port_specs("5432").is_empty());
    }

    #[test]
    fn host_ports_wait_only_for_overlapping_claims() {
        let ports = HostClaims::default();
        let held = ports
            .acquire(PortClaim::Ports(vec![5432]), "a", |_| {}, || false)
            .unwrap();
        assert!(ports
            .acquire(PortClaim::Ports(vec![6379]), "b", |_| panic!(), || false)
            .is_some());
        let mut holder = String::new();
        let stopped = ports.acquire(PortClaim::Any, "c", |h| holder = h.into(), || true);
        assert!(stopped.is_none());
        assert_eq!(holder, "a");
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                ports
                    .acquire(PortClaim::Ports(vec![5432]), "d", |_| {}, || false)
                    .is_some()
            });
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
            assert!(waiter.join().unwrap());
        });
    }

    #[test]
    fn slot_wait_gives_up_when_stopped() {
        let slots = Slots::new(1);
        let held = slots.acquire_unless(|| false).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                slots
                    .acquire_unless(|| stop.load(Ordering::SeqCst))
                    .is_none()
            });
            std::thread::sleep(Duration::from_millis(150));
            stop.store(true, Ordering::SeqCst);
            assert!(waiter.join().unwrap());
        });
        drop(held);
        assert!(slots.acquire_unless(|| true).is_some());
    }

    #[test]
    fn slots_are_granted_in_ticket_order() {
        let slots = Slots::new(1);
        let held = slots.acquire_unless(|| false).unwrap();
        let tickets: Vec<u64> = (0..5).map(|_| slots.queue()).collect();
        let order = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            // Start the waiters last ticket first: the line still decides.
            for (n, &t) in tickets.iter().enumerate().rev() {
                let (slots, order) = (&slots, &order);
                s.spawn(move || {
                    let _slot = slots.acquire_in_turn(t, || false).unwrap();
                    order.lock().unwrap().push(n);
                });
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(held);
        });
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_cancelled_waiter_leaves_the_line() {
        let slots = Slots::new(1);
        let held = slots.acquire_unless(|| false).unwrap();
        let (first, second) = (slots.queue(), slots.queue());
        assert!(slots.acquire_in_turn(first, || true).is_none());
        drop(held);
        assert!(slots.acquire_in_turn(second, || false).is_some());
    }

    #[test]
    fn a_cancelled_waiter_behind_the_head_leaves_the_line() {
        let slots = Slots::new(1);
        let (head, middle, last) = (slots.queue(), slots.queue(), slots.queue());
        assert!(slots.acquire_in_turn(middle, || true).is_none());
        drop(slots.acquire_in_turn(head, || false).unwrap());
        assert!(slots.acquire_in_turn(last, || false).is_some());
    }

    /// With `max-parallel: 2`, a short leg starts as soon as another short one
    /// ends, without waiting for the long leg running beside them.
    #[test]
    fn matrix_legs_fill_freed_places_at_once() {
        let events = Mutex::new(Vec::new());
        let running = std::sync::atomic::AtomicUsize::new(0);
        let most = std::sync::atomic::AtomicUsize::new(0);
        run_legs(4, 2, |i| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            events.lock().unwrap().push(format!("start {i}"));
            std::thread::sleep(Duration::from_millis(if i == 0 { 600 } else { 50 }));
            events.lock().unwrap().push(format!("end {i}"));
            running.fetch_sub(1, Ordering::SeqCst);
        });
        let events = events.into_inner().unwrap();
        let at = |e: &str| events.iter().position(|x| x == e).unwrap();
        assert!(
            at("start 2") < at("end 0") && at("start 3") < at("end 0"),
            "{events:?}"
        );
        assert!(at("start 2") > at("start 1") && at("start 3") > at("start 2"));
        assert_eq!(most.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn multi_line_masks_are_registered_line_by_line() {
        let mut masks = vec![];
        register_mask(&mut masks, "-----BEGIN-----\r\nAAA\n\n  \nBBB");
        register_mask(&mut masks, "AAA");
        assert_eq!(
            masks,
            [
                "-----BEGIN-----\r\nAAA\n\n  \nBBB",
                "-----BEGIN-----",
                "AAA",
                "BBB"
            ]
        );
        assert_eq!(commands::mask("key BBB end", &masks), "key *** end");
    }

    #[test]
    fn paths_under_a_base_resolve_dot_dot_lexically() {
        let base = Path::new("/w/workspace");
        assert_eq!(path_under(base, "a/b").unwrap(), base.join("a/b"));
        assert_eq!(path_under(base, "a/../c/.").unwrap(), base.join("c"));
        assert_eq!(path_under(base, ".").unwrap(), base);
        for escape in ["..", "../x", "a/../../x", "/etc", "/w/workspace/a"] {
            assert_eq!(path_under(base, escape), None, "{escape}");
        }
    }

    #[test]
    fn a_mask_registered_again_is_kept_once() {
        let log = Logger {
            prefix: String::new(),
            workflow: "w".into(),
            job: "j".into(),
            json: false,
            quiet: true,
            masks: Arc::new(Mutex::new(vec![])),
            history: None,
        };
        for _ in 0..1000 {
            log.add_mask("s3cret".into());
        }
        log.add_mask(String::new());
        log.add_mask("other".into());
        assert_eq!(*log.masks.lock().unwrap(), ["s3cret", "other"]);
    }

    #[test]
    fn powershell_and_cmd_scripts_are_wrapped() {
        let ps = wrap_script(Some("pwsh"), "Write-Output hi");
        assert!(ps.starts_with("$ErrorActionPreference = 'stop'\nWrite-Output hi\n"));
        assert!(ps.contains("exit $LASTEXITCODE"));
        assert_eq!(wrap_script(Some("cmd"), "echo hi"), "@echo off\necho hi");
        assert_eq!(wrap_script(Some("bash"), "echo hi"), "echo hi");
        assert_eq!(
            shell_argv(Some("cmd"), Path::new("s.cmd")),
            vec!["cmd", "/D", "/E:ON", "/V:OFF", "/S", "/C", "CALL \"s.cmd\""]
        );
        assert_eq!(shell_ext(Some("cmd")), "cmd");
    }

    #[test]
    fn container_names_carry_the_run_owner() {
        let job = format!("runthrough-build-{}-0", run_tag());
        assert_eq!(owner_pid(&job), Some(std::process::id()));
        let step = format!("runthrough-step-{}-3", run_tag());
        assert_eq!(owner_pid(&step), Some(std::process::id()));
    }

    #[test]
    fn image_os_from_os_release() {
        assert_eq!(
            os_release_image_os("ubuntu 26\n").as_deref(),
            Some("ubuntu26")
        );
        assert_eq!(os_release_image_os("debian 12"), None);
        assert_eq!(os_release_image_os(""), None);
    }

    #[test]
    fn tool_cache_lock_is_taken_in_turn_and_given_up_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/linux.lock");
        let first = lock_tool_cache(&path, || panic!("free"), || false).unwrap();
        assert!(first.is_some());
        let waited = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        std::thread::scope(|s| {
            let second = s.spawn(|| {
                lock_tool_cache(
                    &path,
                    || waited.store(true, Ordering::SeqCst),
                    || cancel.load(Ordering::SeqCst),
                )
                .unwrap()
            });
            while !waited.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            cancel.store(true, Ordering::SeqCst);
            assert!(second.join().unwrap().is_none());
        });
        drop(first);
        assert!(lock_tool_cache(&path, || panic!("released"), || false)
            .unwrap()
            .is_some());
    }

    #[test]
    fn tool_cache_key_follows_the_image_release() {
        assert_eq!(tool_cache_key(Some("ubuntu 22")), "linux-ubuntu22");
        assert_eq!(tool_cache_key(Some("ubuntu 24")), "linux-ubuntu24");
        assert_eq!(tool_cache_key(Some("arch")), "linux-arch");
        assert_eq!(tool_cache_key(Some("../x 1")), "linux-..x1");
        assert_eq!(tool_cache_key(None), "linux");
    }

    #[test]
    fn image_os_names() {
        let os = |l: &[&str]| image_os(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(os(&["ubuntu-22.04"]), "ubuntu22");
        assert_eq!(os(&["self-hosted", "ubuntu-latest"]), "ubuntu24");
        assert_eq!(os(&["windows-2022"]), "windows2022");
        assert_eq!(os(&["linux"]), "");
    }

    #[test]
    fn default_platforms_follow_act_medium_images() {
        let img = |l: &str| default_platform(l).flatten();
        assert_eq!(img("ubuntu-latest").as_deref(), Some(DEFAULT_IMAGE));
        assert_eq!(
            img("ubuntu-22.04").as_deref(),
            Some("catthehacker/ubuntu:act-22.04")
        );
        assert_eq!(
            img("Ubuntu-20.04").as_deref(),
            Some("catthehacker/ubuntu:act-20.04")
        );
        assert_eq!(default_platform("self-hosted"), Some(None));
        assert_eq!(default_platform("windows-latest"), None);
        let err: anyhow::Error = NoPlatform(vec!["windows-latest".into()]).into();
        assert!(err.downcast_ref::<NoPlatform>().is_some());
    }

    #[test]
    fn call_chain_refuses_cycles_and_deep_nesting() {
        let p = |n: &str| PathBuf::from(format!("/r/{n}.yml"));
        assert!(check_call_chain(&[p("a")], &p("b")).is_ok());
        let err = check_call_chain(&[p("a"), p("b")], &p("a")).unwrap_err();
        assert!(err.to_string().contains("a.yml → b.yml → a.yml"), "{err}");
        let nine: Vec<PathBuf> = (0..9).map(|i| p(&i.to_string())).collect();
        assert!(check_call_chain(&nine, &p("x")).is_ok());
        let ten: Vec<PathBuf> = (0..10).map(|i| p(&i.to_string())).collect();
        assert!(check_call_chain(&ten, &p("x")).is_err());
    }

    #[test]
    fn call_args_follow_workflow_call_declarations() {
        let called: Workflow = serde_yaml::from_str(
            "on:\n  workflow_call:\n    inputs:\n      need: {required: true}\n      opt: {required: true, default: x}\n    secrets:\n      Must: {required: true}\njobs: {}\n",
        )
        .unwrap();
        let job =
            |y: &str| -> Job { serde_yaml::from_str(&format!("uses: ./w.yml\n{y}")).unwrap() };
        let ok = job("with: {NEED: 1}\nsecrets: {must: s}");
        assert!(check_call_args(&called, &ok, "w").is_ok());
        assert!(check_call_args(&called, &job("with: {need: 1}\nsecrets: inherit"), "w").is_ok());
        let err = check_call_args(&called, &job("with: {extra: 1}\nsecrets: {other: s}"), "w")
            .unwrap_err()
            .to_string();
        for msg in [
            "input 'extra' is not defined",
            "input 'need' is required",
            "secret 'other' is not defined",
            "secret 'Must' is required",
        ] {
            assert!(err.contains(msg), "{err}");
        }
        assert!(!err.contains("'opt'"), "{err}");
    }
}
