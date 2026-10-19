//! Run history for the dashboard: every run records its (masked) log lines under
//! `<cache>/runs/<id>/`, and `Summary` rebuilds jobs and steps from the log's
//! status markers (`⭐` step start, `✅`/`❌`/`⚠️`/`🛑` step end, `⏭️` skipped,
//! `🚀`/`🏁` job start and end).
use crate::markers;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runs kept on disk; older ones are removed when a run starts.
const KEEP: usize = 100;

/// How often buffered log lines reach the file, so a live dashboard keeps up.
const FLUSH_EVERY: Duration = Duration::from_millis(200);

pub fn runs_dir(cache: &Path) -> PathBuf {
    cache.join("runs")
}

/// Appends one run's log; shared by every job's logger.
pub struct Recorder {
    dir: PathBuf,
    log: Arc<Mutex<BufWriter<std::fs::File>>>,
}

impl Recorder {
    /// Create `<cache>/runs/<id>/` with `meta.json` (meta plus `id`, `started`, `pid`).
    pub fn start(cache: &Path, mut meta: Map<String, Value>) -> Result<Recorder> {
        let runs = runs_dir(cache);
        std::fs::create_dir_all(&runs).with_context(|| format!("creating {}", runs.display()))?;
        gc(&runs);
        let now = now_secs();
        // Zero-padded milliseconds first, so directory names sort by start time.
        let id = format!("{:015}-{}", (now * 1000.0) as u64, std::process::id());
        let dir = runs.join(&id);
        std::fs::create_dir_all(&dir)?;
        meta.insert("id".into(), json!(id));
        meta.insert("started".into(), json!(now));
        meta.insert("pid".into(), json!(std::process::id()));
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        let log = Arc::new(Mutex::new(BufWriter::new(std::fs::File::create(
            dir.join("log.jsonl"),
        )?)));
        // Lines are written in batches instead of one write per line under the
        // shared lock; this thread ends with the recorder.
        let weak = Arc::downgrade(&log);
        std::thread::spawn(move || loop {
            std::thread::sleep(FLUSH_EVERY);
            let Some(log) = weak.upgrade() else { break };
            let _ = log.lock().unwrap().flush();
        });
        Ok(Recorder { dir, log })
    }

    /// One log line of a job (`msg`), or another record such as a job's result.
    pub fn record(&self, mut v: Value) {
        v["t"] = json!(now_secs());
        let mut line = v.to_string();
        line.push('\n');
        // A full disk must not fail the run: history is best effort.
        let _ = self.log.lock().unwrap().write_all(line.as_bytes());
    }

    pub fn finish(&self, ok: bool) {
        // The whole log is on disk before the result marks the run finished.
        let _ = self.log.lock().unwrap().flush();
        let _ = std::fs::write(
            self.dir.join("result.json"),
            json!({"finished": now_secs(), "ok": ok}).to_string(),
        );
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.log.lock().unwrap().flush();
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn gc(runs: &Path) {
    let mut ids = list_ids(runs);
    if ids.len() >= KEEP {
        ids.truncate(ids.len() + 1 - KEEP);
        for id in ids {
            let _ = std::fs::remove_dir_all(runs.join(id));
        }
    }
}

/// Run IDs, oldest first.
fn list_ids(runs: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(runs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("meta.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| valid_id(id))
        .collect();
    ids.sort();
    ids
}

/// IDs come from URLs: only the shape `Recorder::start` makes names a run.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A run's log read so far: the next byte to read and a summary of what came before.
struct Tail {
    offset: u64,
    summary: Summary,
}

impl Tail {
    fn new(keep_lines: bool) -> Tail {
        Tail {
            offset: 0,
            summary: Summary::new(keep_lines),
        }
    }

    /// Feed the complete lines the log gained since the last call.
    fn update(&mut self, log: &Path) {
        let Ok(mut f) = std::fs::File::open(log) else {
            return;
        };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < self.offset {
            *self = Tail::new(self.summary.keep_lines);
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = std::io::BufReader::new(f.take(len - self.offset));
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                // The last line may be half written while the run goes on: it is
                // read again, whole, next time.
                Ok(n) if n > 0 && buf.ends_with(b"\n") => {
                    self.offset += n as u64;
                    if let Ok(r) = serde_json::from_slice::<Value>(&buf) {
                        self.summary.feed(&r);
                    }
                }
                _ => break,
            }
        }
    }
}

/// How many logs with step output a dashboard keeps parsed: the job pages
/// viewed most recently.
const FULL_KEPT: usize = 4;

/// Run summaries kept between dashboard requests, so a poll reads only what a
/// log gained since the previous one.
pub struct Store {
    cache: PathBuf,
    /// Per run, without step output: for the runs list and a run's page.
    brief: Mutex<HashMap<String, Tail>>,
    /// With step output, for job pages; the most recently used last.
    full: Mutex<Vec<(String, Tail)>>,
    /// Runs whose recording process was seen gone: it does not come back.
    dead: Mutex<HashSet<String>>,
}

impl Store {
    pub fn new(cache: &Path) -> Store {
        Store {
            cache: cache.to_path_buf(),
            brief: Mutex::default(),
            full: Mutex::default(),
            dead: Mutex::default(),
        }
    }

    /// `success` / `failure` once finished; `running` while the recording process
    /// lives, `interrupted` when it died without finishing.
    fn status(&self, id: &str, dir: &Path, meta: &Value) -> (String, Option<f64>) {
        if let Some(r) = read_json(&dir.join("result.json")) {
            let s = if r["ok"].as_bool() == Some(true) {
                "success"
            } else {
                "failure"
            };
            return (s.into(), r["finished"].as_f64());
        }
        let alive = meta["pid"].as_u64().is_some_and(|pid| {
            pid == u64::from(std::process::id())
                || (!self.dead.lock().unwrap().contains(id) && process_alive(pid))
        });
        if !alive {
            self.dead.lock().unwrap().insert(id.to_string());
        }
        (if alive { "running" } else { "interrupted" }.into(), None)
    }

    /// The newest runs, for the runs list.
    pub fn list(&self, limit: usize) -> Vec<Value> {
        let runs = runs_dir(&self.cache);
        let mut ids = list_ids(&runs);
        ids.reverse();
        ids.truncate(limit);
        let mut brief = self.brief.lock().unwrap();
        brief.retain(|id, _| ids.contains(id));
        ids.iter()
            .filter_map(|id| {
                let dir = runs.join(id);
                let meta = read_json(&dir.join("meta.json"))?;
                let (status, finished) = self.status(id, &dir, &meta);
                let tail = brief.entry(id.clone()).or_insert_with(|| Tail::new(false));
                tail.update(&dir.join("log.jsonl"));
                let failed: Vec<Value> = tail
                    .summary
                    .jobs(&[])
                    .into_iter()
                    .filter(|j| j["status"] == "failure")
                    .map(|j| json!({"workflow": j["workflow"], "job": j["job"], "error": j["error"]}))
                    .collect();
                Some(json!({"meta": meta, "status": status, "finished": finished, "failed": failed}))
            })
            .collect()
    }

    /// One run: its meta, status and jobs with their steps (line counts, not output).
    pub fn run(&self, id: &str) -> Option<Value> {
        let dir = runs_dir(&self.cache).join(id);
        let meta = read_json(&dir.join("meta.json"))?;
        let (status, finished) = self.status(id, &dir, &meta);
        let mut brief = self.brief.lock().unwrap();
        let tail = brief
            .entry(id.to_string())
            .or_insert_with(|| Tail::new(false));
        tail.update(&dir.join("log.jsonl"));
        let jobs = tail.summary.jobs(&[]);
        let run_names = tail.summary.run_names.clone();
        Some(
            json!({"meta": meta, "status": status, "finished": finished, "jobs": jobs, "run_names": run_names}),
        )
    }

    /// One job (matrix leg) of a run, with its steps' output: step `i` from line
    /// `skip[i]` on (all of it for steps `skip` does not reach), and in `from`
    /// where its lines start.
    pub fn job(&self, id: &str, workflow: &str, job: &str, skip: &[usize]) -> Option<Value> {
        let log = runs_dir(&self.cache).join(id).join("log.jsonl");
        let mut full = self.full.lock().unwrap();
        let mut tail = match full.iter().position(|(k, _)| k == id) {
            Some(i) => full.remove(i).1,
            None => Tail::new(true),
        };
        tail.update(&log);
        let found = tail.summary.job(workflow, job, skip);
        full.push((id.to_string(), tail));
        if full.len() > FULL_KEPT {
            full.remove(0);
        }
        found
    }
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
pub(crate) fn process_alive(_pid: u64) -> bool {
    false
}

/// Jobs in the order they started, rebuilt from the log's status markers one
/// record at a time.
pub struct Summary {
    jobs: Vec<Job>,
    results: Vec<(String, String, String)>,
    run_names: Vec<Value>,
    /// Without it each step keeps only its line count and last line.
    keep_lines: bool,
}

impl Summary {
    pub fn new(keep_lines: bool) -> Summary {
        Summary {
            jobs: vec![],
            results: vec![],
            run_names: vec![],
            keep_lines,
        }
    }

    pub fn feed(&mut self, r: &Value) {
        if r.get("run_name").is_some() {
            self.run_names
                .push(json!({"workflow": r["wf"], "run_name": r["run_name"]}));
        }
        let (Some(wf), Some(job)) = (r["wf"].as_str(), r["job"].as_str()) else {
            return;
        };
        if let Some(result) = r["result"].as_str() {
            self.results.push((wf.into(), job.into(), result.into()));
            return;
        }
        let Some(msg) = r["msg"].as_str() else {
            return;
        };
        // The concurrency logger reports on the workflow, not on a job.
        if job == "concurrency" && !self.jobs.iter().any(|j| j.workflow == wf && j.job == job) {
            return;
        }
        let t = r["t"].as_f64().unwrap_or(0.0);
        let keep_lines = self.keep_lines;
        let j = match self
            .jobs
            .iter()
            .position(|j| j.workflow == wf && j.job == job)
        {
            Some(i) => &mut self.jobs[i],
            None => {
                self.jobs.push(Job::new(wf, job, t, keep_lines));
                self.jobs.last_mut().unwrap()
            }
        };
        j.feed(msg, t);
    }

    /// Every job, with each step's output as in `Store::job`.
    pub fn jobs(&self, skip: &[usize]) -> Vec<Value> {
        self.jobs.iter().map(|j| self.value(j, skip)).collect()
    }

    fn job(&self, workflow: &str, job: &str, skip: &[usize]) -> Option<Value> {
        self.jobs
            .iter()
            .find(|j| j.workflow == workflow && j.job == job)
            .map(|j| self.value(j, skip))
    }

    fn value(&self, j: &Job, skip: &[usize]) -> Value {
        // A leg's result also comes from the workflow's summary (`test` for `test (1)`).
        let id = j.job.split(" (").next().unwrap_or(&j.job);
        let summary = self
            .results
            .iter()
            .find(|(w, k, _)| *w == j.workflow && (*k == j.job || *k == id))
            .map(|(_, _, r)| r.as_str());
        j.to_value(id, summary, skip)
    }
}

struct Step {
    name: String,
    status: &'static str,
    started: f64,
    secs: Option<f64>,
    /// Every line, or only the last one when the summary keeps no output.
    lines: Vec<String>,
    count: usize,
}

struct Job {
    workflow: String,
    job: String,
    started: f64,
    runs_on: Option<String>,
    status: String,
    secs: Option<f64>,
    error: Option<String>,
    steps: Vec<Step>,
    keep_lines: bool,
}

impl Job {
    fn new(workflow: &str, job: &str, t: f64, keep_lines: bool) -> Job {
        Job {
            workflow: workflow.into(),
            job: job.into(),
            started: t,
            runs_on: None,
            status: "queued".into(),
            secs: None,
            error: None,
            steps: vec![],
            keep_lines,
        }
    }

    fn push(&mut self, name: &str, status: &'static str, t: f64) {
        self.steps.push(Step {
            name: name.into(),
            status,
            started: t,
            secs: None,
            lines: vec![],
            count: 0,
        });
    }

    /// End the newest running step called `name`.
    fn end(&mut self, name: &str, status: &'static str, secs: Option<f64>) -> bool {
        match self
            .steps
            .iter_mut()
            .rev()
            .find(|s| s.status == "running" && s.name == name)
        {
            Some(s) => {
                s.status = status;
                s.secs = secs;
                if status == "failure" && self.error.is_none() {
                    self.error = Some(match s.lines.last() {
                        Some(l) => format!("{name}: {}", l.trim()),
                        None => name.to_string(),
                    });
                }
                true
            }
            None => false,
        }
    }

    /// Pre and post steps print no end marker: the next step or the job's end closes them.
    fn close_hooks(&mut self) {
        for s in &mut self.steps {
            if s.status == "running" && (s.name.starts_with("Pre ") || s.name.starts_with("Post "))
            {
                s.status = "success";
            }
        }
    }

    /// Output goes to the newest running step, else to a set-up or complete pseudo step.
    fn output(&mut self, line: &str, t: f64) {
        if !self.steps.iter().any(|s| s.status == "running") {
            let name = if self.steps.is_empty() {
                "Set up job"
            } else {
                "Complete job"
            };
            self.push(name, "running", t);
        }
        let keep_lines = self.keep_lines;
        if let Some(s) = self.steps.iter_mut().rev().find(|s| s.status == "running") {
            if !keep_lines {
                s.lines.clear();
            }
            s.lines.push(line.to_string());
            s.count += 1;
        }
    }

    fn feed(&mut self, msg: &str, t: f64) {
        if let Some(desc) = markers::parse_job_start(msg) {
            self.runs_on = Some(desc.to_string());
            self.status = "running".into();
            self.output(msg, t);
            if let Some(s) = self.steps.iter_mut().find(|s| s.name == "Set up job") {
                s.status = "success";
            }
            return;
        }
        if let Some((result, secs)) = markers::parse_job_end(msg) {
            self.close_hooks();
            self.status = result.to_string();
            self.secs = secs;
            self.output(msg, t);
            for s in &mut self.steps {
                if s.status == "running" {
                    s.status = if s.name == "Complete job" {
                        "success"
                    } else {
                        "cancelled"
                    };
                }
            }
            return;
        }
        if let Some(why) = markers::parse_job_skipped(msg) {
            self.status = "skipped".into();
            self.error =
                Some(why.trim_start_matches([':', ' ']).to_string()).filter(|s| !s.is_empty());
            return;
        }
        if let Some(name) = markers::parse_step_start(msg) {
            self.close_hooks();
            // Output between steps went to a pseudo step; a real step closes it.
            for s in &mut self.steps {
                if s.status == "running" && matches!(s.name.as_str(), "Set up job" | "Complete job")
                {
                    s.status = "success";
                }
            }
            self.push(name, "running", t);
            return;
        }
        if let Some(name) = markers::parse_step_skipped(msg) {
            self.push(name, "skipped", t);
            return;
        }
        if let Some((name, st, secs)) = markers::parse_step_end(msg) {
            if self.end(name, st, secs) {
                return;
            }
        }
        if msg.starts_with("❌ post step failed") {
            if let Some(s) = self
                .steps
                .iter_mut()
                .rev()
                .find(|s| s.status == "running" && s.name.starts_with("Post "))
            {
                s.status = "failure";
            }
        }
        if let Some(e) = msg.strip_prefix("❌ ") {
            self.error.get_or_insert_with(|| e.to_string());
        } else if let Some(e) = msg.strip_prefix("  ❗ ") {
            self.error.get_or_insert_with(|| e.to_string());
        }
        // `  | ` marks a step's own output; other indented lines are runner notes.
        let line = msg
            .strip_prefix("  | ")
            .or_else(|| msg.strip_prefix("  "))
            .unwrap_or(msg);
        self.output(line, t);
    }

    /// Steps carry their output as in `Store::job`, or only its line count
    /// when the summary keeps no output.
    fn to_value(&self, id: &str, summary: Option<&str>, skip: &[usize]) -> Value {
        let status = match (summary, self.status.as_str()) {
            // A workflow's summary is final; the job's own line covers legs.
            (Some(r), "running" | "queued") => r,
            _ => self.status.as_str(),
        };
        let steps: Vec<Value> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut v = json!({"name": s.name, "status": s.status, "started": s.started, "secs": s.secs});
                if self.keep_lines {
                    let from = skip.get(i).copied().unwrap_or(0).min(s.lines.len());
                    v["lines"] = json!(s.lines[from..]);
                    v["from"] = json!(from);
                } else {
                    v["lines"] = json!(s.count);
                }
                v
            })
            .collect();
        json!({
            "workflow": self.workflow, "job": self.job, "id": id, "status": status,
            "started": self.started, "secs": self.secs, "runs_on": self.runs_on,
            "error": self.error, "steps": steps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(job: &str, msg: &str) -> Value {
        json!({"t": 1.0, "wf": "demo", "job": job, "msg": msg})
    }

    fn summarize_jobs(log: &[Value]) -> Vec<Value> {
        let mut s = Summary::new(true);
        for r in log {
            s.feed(r);
        }
        s.jobs(&[])
    }

    #[test]
    fn rebuilds_steps_and_results_from_markers() {
        let log = vec![
            rec(
                "build",
                "🚀 start job on docker:catthehacker/ubuntu:act-latest",
            ),
            rec("build", "⭐ Run actions/checkout@v4"),
            rec("build", "  copied /repo → /ws"),
            rec("build", "  ✅ Run actions/checkout@v4 (0.3s)"),
            rec("build", "⭐ Run actions/setup-python@v5"),
            rec("build", "  | Extract downloaded archive"),
            rec(
                "build",
                "  ❗ The process '/usr/bin/tar' failed with exit code 2",
            ),
            rec("build", "  ❌ Run actions/setup-python@v5 (12.5s)"),
            rec("build", "⏭️  Run actions/setup-node@v4"),
            rec("build", "❌ job failure in 12.9s"),
            rec("test (2)", "🚀 start job on host"),
            rec("test (2)", "⭐ Run exit 1"),
            rec("test (2)", "  ⚠️  Run exit 1 failed, continuing (0.0s)"),
            rec("test (2)", "🏁 job success in 0.0s"),
            json!({"t": 2.0, "wf": "demo", "job": "test", "result": "success"}),
            rec("report", "⏭️  job skipped (if condition is false)"),
        ];
        let jobs = summarize_jobs(&log);
        let build = &jobs[0];
        assert_eq!(build["status"], "failure");
        assert_eq!(build["secs"], 12.9);
        assert_eq!(build["runs_on"], "docker:catthehacker/ubuntu:act-latest");
        assert_eq!(
            build["error"],
            "The process '/usr/bin/tar' failed with exit code 2"
        );
        let names: Vec<_> = build["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            names,
            [
                ("Set up job", "success"),
                ("Run actions/checkout@v4", "success"),
                ("Run actions/setup-python@v5", "failure"),
                ("Run actions/setup-node@v4", "skipped"),
                ("Complete job", "success"),
            ]
        );
        assert_eq!(
            build["steps"][2]["lines"],
            json!([
                "Extract downloaded archive",
                "❗ The process '/usr/bin/tar' failed with exit code 2"
            ])
        );
        assert_eq!(build["steps"][2]["secs"], 12.5);
        assert_eq!(jobs[1]["id"], "test");
        assert_eq!(jobs[1]["steps"][1]["status"], "warning");
        assert_eq!(jobs[2]["status"], "skipped");
    }

    #[test]
    fn pre_and_post_steps_end_without_a_marker() {
        let log = vec![
            rec("js", "🚀 start job on host"),
            rec("js", "⭐ Pre Run ./a"),
            rec("js", "⭐ Run exit 3"),
            rec("js", "  exit code 3"),
            rec("js", "  ❌ Run exit 3 (0.0s)"),
            rec("js", "⭐ Post Run ./a"),
            rec("js", "  | post sees state"),
            rec("js", "⭐ Post Run ./b"),
            rec("js", "❌ post step failed (exit 2)"),
            rec("js", "❌ job failure in 0.1s"),
        ];
        let job = &summarize_jobs(&log)[0];
        let steps: Vec<_> = job["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            steps,
            [
                ("Set up job", "success"),
                ("Pre Run ./a", "success"),
                ("Run exit 3", "failure"),
                ("Post Run ./a", "success"),
                ("Post Run ./b", "failure"),
                ("Complete job", "success"),
            ]
        );
        assert_eq!(job["error"], "Run exit 3: exit code 3");
        assert_eq!(job["steps"][2]["lines"], json!(["exit code 3"]));
    }

    #[test]
    fn a_running_job_keeps_its_open_step() {
        let log = vec![
            rec("build", "🚀 start job on host"),
            rec("build", "⭐ Run sleep 9"),
            rec("build", "  | tick"),
        ];
        let jobs = summarize_jobs(&log);
        assert_eq!(jobs[0]["status"], "running");
        assert_eq!(jobs[0]["steps"][1]["status"], "running");
        assert_eq!(jobs[0]["steps"][1]["lines"], json!(["tick"]));
    }

    #[test]
    fn records_runs_and_keeps_ids_safe() {
        let cache = tempfile::tempdir().unwrap();
        let mut meta = Map::new();
        meta.insert("event".into(), json!("push"));
        let r = Recorder::start(cache.path(), meta).unwrap();
        r.record(json!({"wf": "demo", "job": "build", "msg": "🚀 start job on host"}));
        r.record(json!({"wf": "demo", "job": "build", "msg": "🏁 job success in 0.1s"}));
        let store = Store::new(cache.path());
        let listed = store.list(10);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["status"], "running");
        r.finish(true);
        let id = listed[0]["meta"]["id"].as_str().unwrap();
        let one = store.run(id).unwrap();
        assert_eq!(one["status"], "success");
        assert_eq!(one["jobs"][0]["status"], "success");
        assert!(valid_id(id));
        assert!(!valid_id("../etc"));
        assert!(!valid_id(""));
    }

    /// A log written by hand, as `Recorder` would, without a recording process.
    fn write_run(cache: &Path, id: &str, records: &[Value]) -> PathBuf {
        let dir = runs_dir(cache).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            json!({"id": id, "pid": std::process::id()}).to_string(),
        )
        .unwrap();
        let mut log = String::new();
        for r in records {
            log.push_str(&r.to_string());
            log.push('\n');
        }
        std::fs::write(dir.join("log.jsonl"), log).unwrap();
        dir
    }

    fn append(dir: &Path, s: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("log.jsonl"))
            .unwrap()
            .write_all(s.as_bytes())
            .unwrap();
    }

    #[test]
    fn polls_read_only_what_a_log_gained() {
        let cache = tempfile::tempdir().unwrap();
        let mut records = vec![
            rec("build", "🚀 start job on host"),
            rec("build", "⭐ Run make"),
        ];
        records.extend((0..1000).map(|n| rec("build", &format!("  | line {n}"))));
        let dir = write_run(cache.path(), "1-1", &records);
        let store = Store::new(cache.path());
        assert_eq!(
            store.run("1-1").unwrap()["jobs"][0]["steps"][1]["lines"],
            1000
        );
        assert_eq!(store.brief.lock().unwrap()["1-1"].offset, {
            std::fs::metadata(dir.join("log.jsonl")).unwrap().len()
        });
        // A half-written line waits for its end; the steps keep only counts.
        let next = rec("build", "  | line 1000").to_string();
        let (head, rest) = next.split_at(10);
        append(&dir, head);
        assert_eq!(
            store.run("1-1").unwrap()["jobs"][0]["steps"][1]["lines"],
            1000
        );
        append(&dir, &format!("{rest}\n"));
        let run = store.run("1-1").unwrap();
        assert_eq!(run["jobs"][0]["steps"][1]["lines"], 1001);
        assert!(
            store.brief.lock().unwrap()["1-1"].summary.jobs[0].steps[1]
                .lines
                .len()
                <= 1
        );
        // The runs list shares the same summary.
        assert_eq!(store.list(10)[0]["status"], "running");
    }

    #[test]
    fn job_pages_send_only_new_lines() {
        let cache = tempfile::tempdir().unwrap();
        let dir = write_run(
            cache.path(),
            "1-1",
            &[
                rec("build", "🚀 start job on host"),
                rec("build", "⭐ Run make"),
                rec("build", "  | a"),
                rec("build", "  | b"),
            ],
        );
        let store = Store::new(cache.path());
        let first = store.job("1-1", "demo", "build", &[]).unwrap();
        assert_eq!(first["steps"][1]["lines"], json!(["a", "b"]));
        append(&dir, &format!("{}\n", rec("build", "  | c")));
        let next = store.job("1-1", "demo", "build", &[1, 2]).unwrap();
        assert_eq!(next["steps"][0]["lines"], json!([]));
        assert_eq!(next["steps"][1]["lines"], json!(["c"]));
        assert_eq!(next["steps"][1]["from"], 2);
        // Without a count for it, a step sends all of its lines.
        assert_eq!(
            store.job("1-1", "demo", "build", &[1]).unwrap()["steps"][1]["lines"],
            json!(["a", "b", "c"])
        );
        assert!(store.job("1-1", "demo", "other", &[]).is_none());
    }

    #[test]
    fn a_dead_recorder_is_checked_once() {
        let cache = tempfile::tempdir().unwrap();
        let dir = write_run(cache.path(), "1-1", &[]);
        // A PID no process has: the largest a 32-bit pid_t can hold.
        std::fs::write(
            dir.join("meta.json"),
            json!({"id": "1-1", "pid": 2_147_483_647u64}).to_string(),
        )
        .unwrap();
        let store = Store::new(cache.path());
        assert_eq!(store.list(10)[0]["status"], "interrupted");
        assert!(store.dead.lock().unwrap().contains("1-1"));
        assert_eq!(store.list(10)[0]["status"], "interrupted");
    }

    #[test]
    fn the_log_reaches_the_file_by_finish() {
        let cache = tempfile::tempdir().unwrap();
        let r = Recorder::start(cache.path(), Map::new()).unwrap();
        for n in 0..100 {
            r.record(json!({"wf": "demo", "job": "build", "msg": format!("  | {n}")}));
        }
        r.finish(true);
        let log = std::fs::read_to_string(r.dir.join("log.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 100);
    }
}
