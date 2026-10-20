//! Workflow file model (`.github/workflows/*.yml`).

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value as Yaml;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Workflow {
    pub name: Option<String>,
    #[serde(rename = "run-name")]
    pub run_name: Option<String>,
    #[serde(rename = "on", alias = "true", default)]
    pub on: Yaml,
    #[serde(default)]
    pub env: IndexMap<String, Yaml>,
    #[serde(default)]
    pub defaults: Option<Defaults>,
    #[serde(default)]
    pub concurrency: Yaml,
    #[serde(default)]
    pub permissions: Yaml,
    #[serde(rename = "cache-mode")]
    pub cache_mode: Option<String>,
    pub jobs: IndexMap<String, Job>,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Defaults {
    pub run: Option<RunDefaults>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RunDefaults {
    pub shell: Option<String>,
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Job {
    pub name: Option<String>,
    #[serde(rename = "runs-on", default)]
    pub runs_on: Yaml,
    #[serde(default)]
    pub needs: Yaml,
    #[serde(rename = "if")]
    pub if_cond: Option<Yaml>,
    #[serde(default)]
    pub env: IndexMap<String, Yaml>,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub strategy: Option<Strategy>,
    #[serde(default)]
    pub outputs: IndexMap<String, String>,
    pub container: Option<Yaml>,
    pub defaults: Option<Defaults>,
    #[serde(rename = "continue-on-error", default)]
    pub continue_on_error: Yaml,
    #[serde(rename = "timeout-minutes")]
    pub timeout_minutes: Option<Yaml>,
    /// Reusable workflow call: `./path.yml` or `owner/repo/path.yml@ref`.
    pub uses: Option<String>,
    /// Inputs for a reusable workflow call.
    #[serde(default)]
    pub with: IndexMap<String, Yaml>,
    /// Secrets for a reusable workflow call: a mapping, or `inherit`.
    #[serde(default)]
    pub secrets: Yaml,
    #[serde(default)]
    pub services: IndexMap<String, Yaml>,
    #[serde(default)]
    pub environment: Yaml,
    #[serde(default)]
    pub concurrency: Yaml,
    #[serde(default)]
    pub permissions: Yaml,
    #[serde(rename = "cache-mode")]
    pub cache_mode: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Strategy {
    #[serde(default)]
    pub matrix: Yaml,
    #[serde(rename = "fail-fast")]
    pub fail_fast: Option<Yaml>,
    #[serde(rename = "max-parallel")]
    pub max_parallel: Option<Yaml>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "if")]
    pub if_cond: Option<Yaml>,
    pub uses: Option<String>,
    pub run: Option<String>,
    pub shell: Option<String>,
    #[serde(default)]
    pub with: IndexMap<String, Yaml>,
    #[serde(default)]
    pub env: IndexMap<String, Yaml>,
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,
    #[serde(rename = "continue-on-error", default)]
    pub continue_on_error: Yaml,
    #[serde(rename = "timeout-minutes")]
    pub timeout_minutes: Option<Yaml>,
    /// Run without blocking the next step, until a `wait` / `wait-all` /
    /// `cancel` step or the end of the job.
    #[serde(default)]
    pub background: Yaml,
    /// Background step IDs to wait for: one, or a list.
    pub wait: Option<Yaml>,
    #[serde(rename = "wait-all", default, deserialize_with = "present")]
    pub wait_all: Option<Yaml>,
    /// A background step ID to stop.
    pub cancel: Option<String>,
    /// Steps run as background steps, then waited for.
    pub parallel: Option<Vec<Step>>,
}

/// `Some` whenever the key is present, even as `key:` with no value.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Yaml>, D::Error> {
    Yaml::deserialize(d).map(Some)
}

impl Step {
    /// A step that starts or controls background steps.
    pub fn is_control(&self) -> bool {
        !matches!(self.background, Yaml::Null | Yaml::Bool(false))
            || self.wait.is_some()
            || self.wait_all.is_some()
            || self.cancel.is_some()
            || self.parallel.is_some()
    }

    /// Each step of `steps` that runs a command or an action, with its place:
    /// a top-level step's index, and a `parallel` child's index within its group.
    pub fn leaves(steps: &[Step]) -> impl Iterator<Item = ((usize, Option<usize>), &Step)> {
        steps
            .iter()
            .enumerate()
            .flat_map(|(i, step)| match &step.parallel {
                Some(group) => group
                    .iter()
                    .enumerate()
                    .map(|(j, child)| ((i, Some(j)), child))
                    .collect::<Vec<_>>(),
                None => vec![((i, None), step)],
            })
    }

    /// The `steps` context ID of a step without `id`, at `place` (see `leaves`).
    pub fn default_id(place: (usize, Option<usize>)) -> String {
        match place {
            (i, None) => format!("__{i}"),
            (i, Some(j)) => format!("__{i}_{j}"),
        }
    }

    /// IDs listed by a `wait` step.
    pub fn wait_ids(&self) -> Vec<String> {
        match &self.wait {
            Some(Yaml::Sequence(s)) => s.iter().map(yaml_scalar_to_string).collect(),
            Some(v) => vec![yaml_scalar_to_string(v)],
            None => vec![],
        }
    }
}

impl Workflow {
    /// Load the workflow in `source` as the file at `path` (`--committed`
    /// reads a repository's workflows from a copy of its HEAD commit).
    pub fn load_as(source: &Path, path: &Path) -> Result<Workflow> {
        let text = std::fs::read_to_string(source)
            .with_context(|| format!("reading {}", path.display()))?;
        let doc: Yaml =
            serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let errors = schema_errors(&doc);
        if !errors.is_empty() {
            bail!("{}:\n  {}", path.display(), errors.join("\n  "));
        }
        let mut wf: Workflow =
            serde_yaml::from_value(doc).with_context(|| format!("parsing {}", path.display()))?;
        wf.path = path.to_path_buf();
        Ok(wf)
    }

    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }

    /// Event names this workflow is triggered by.
    pub fn events(&self) -> Vec<String> {
        match &self.on {
            Yaml::String(s) => vec![s.clone()],
            Yaml::Sequence(seq) => seq
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            Yaml::Mapping(m) => m
                .keys()
                .filter_map(|k| k.as_str().map(String::from))
                .collect(),
            _ => vec![],
        }
    }

    /// `on.<event>.<section>` (`inputs`, `secrets`, `outputs`): name → declaration.
    pub fn section(&self, event: &str, section: &str) -> Option<&serde_yaml::Mapping> {
        self.on
            .get(event)
            .and_then(|d| d.get(section))
            .and_then(Yaml::as_mapping)
    }

    /// `f`'s value for each declaration in `on.<event>.<section>` that has one,
    /// by name (declarations with non-string names are left out).
    fn each_declared<T>(
        &self,
        event: &str,
        section: &str,
        f: impl Fn(&Yaml) -> Option<T>,
    ) -> IndexMap<String, T> {
        self.section(event, section)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.as_str()?.to_string(), f(v)?)))
            .collect()
    }

    /// `on.<event>.inputs.<name>.default` values.
    pub fn input_defaults(&self, event: &str) -> IndexMap<String, Yaml> {
        self.each_declared(event, "inputs", |v| v.get("default").cloned())
    }

    /// `on.<event>.inputs.<name>.type` values.
    pub fn input_types(&self, event: &str) -> IndexMap<String, String> {
        self.each_declared(event, "inputs", |v| {
            v.get("type").and_then(Yaml::as_str).map(String::from)
        })
    }

    /// `on.<event>.<section>` declarations (`inputs`, `secrets`): name → `required`.
    pub fn declared(&self, event: &str, section: &str) -> IndexMap<String, bool> {
        self.each_declared(event, section, |v| {
            Some(v.get("required").and_then(Yaml::as_bool).unwrap_or(false))
        })
    }

    /// `on.workflow_call.outputs`: name → value expression.
    pub fn call_outputs(&self) -> IndexMap<String, String> {
        self.each_declared("workflow_call", "outputs", |v| {
            v.get("value").map(yaml_scalar_to_string)
        })
    }
}

impl Job {
    pub fn needs(&self) -> Vec<String> {
        match &self.needs {
            Yaml::String(s) => vec![s.clone()],
            Yaml::Sequence(seq) => seq
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => vec![],
        }
    }
}

/// Discover workflow files in a directory (or return the single file given).
/// With `recurse`, subdirectories are searched too (as act does).
pub fn discover(path: &Path, recurse: bool) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        bail!("reading workflow directory {}: not found", path.display());
    }
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(path)
        .max_depth(if recurse { usize::MAX } else { 1 })
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yml" | "yaml")))
        .collect();
    files.sort();
    Ok(files)
}

/// Shape of a value in the workflow schema.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Map,
    Seq,
    /// A string or a list of strings (`needs`, `runs-on` labels).
    StrOrSeq,
    /// A scalar (string, number, bool), e.g. `if` or `timeout-minutes`.
    Scalar,
    /// A mapping, or a string holding an expression (`env: ${{ fromJSON(...) }}`).
    MapOrExpr,
    /// `runs-on`, `container`, `concurrency`, `environment`: string or mapping (or list).
    Any,
}

fn kind_ok(v: &Yaml, k: Kind) -> bool {
    let scalar = matches!(v, Yaml::String(_) | Yaml::Number(_) | Yaml::Bool(_));
    match k {
        Kind::Str => matches!(v, Yaml::String(_)),
        Kind::Map => matches!(v, Yaml::Mapping(_)),
        Kind::Seq => matches!(v, Yaml::Sequence(_)),
        Kind::StrOrSeq => matches!(v, Yaml::String(_) | Yaml::Sequence(_)),
        Kind::Scalar => scalar,
        Kind::MapOrExpr => matches!(v, Yaml::Mapping(_) | Yaml::String(_)),
        Kind::Any => !matches!(v, Yaml::Null),
    }
}

const JOB_TYPES: &[(&str, Kind)] = &[
    ("name", Kind::Scalar),
    ("needs", Kind::StrOrSeq),
    ("if", Kind::Scalar),
    ("runs-on", Kind::Any),
    ("outputs", Kind::Map),
    ("env", Kind::MapOrExpr),
    ("defaults", Kind::Map),
    ("steps", Kind::Seq),
    ("timeout-minutes", Kind::Scalar),
    ("strategy", Kind::MapOrExpr),
    ("continue-on-error", Kind::Scalar),
    ("container", Kind::Any),
    ("services", Kind::Map),
    ("uses", Kind::Str),
    ("with", Kind::Map),
    ("secrets", Kind::MapOrExpr),
    ("permissions", Kind::MapOrExpr),
    ("environment", Kind::Any),
    ("concurrency", Kind::Any),
    ("cache-mode", Kind::Str),
    ("snapshot", Kind::Any),
];
const WORKFLOW_TYPES: &[(&str, Kind)] = &[
    ("name", Kind::Scalar),
    ("run-name", Kind::Scalar),
    ("env", Kind::MapOrExpr),
    ("defaults", Kind::Map),
    ("permissions", Kind::MapOrExpr),
    ("concurrency", Kind::Any),
    ("cache-mode", Kind::Str),
    // Checked by hand in `schema_errors`.
    ("on", Kind::Any),
    ("jobs", Kind::Any),
];
const STEP_TYPES: &[(&str, Kind)] = &[
    ("id", Kind::Str),
    ("if", Kind::Scalar),
    ("name", Kind::Scalar),
    ("uses", Kind::Str),
    ("run", Kind::Scalar),
    ("working-directory", Kind::Str),
    ("shell", Kind::Str),
    ("with", Kind::Map),
    ("env", Kind::MapOrExpr),
    ("continue-on-error", Kind::Scalar),
    ("timeout-minutes", Kind::Scalar),
    ("background", Kind::Scalar),
    ("wait", Kind::StrOrSeq),
    ("cancel", Kind::Str),
    ("parallel", Kind::Seq),
    ("wait-all", Kind::Any),
];

/// A job's `cache-mode`: whether its steps may restore caches, save them, both
/// (`write`, the default) or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    Read,
    Write,
    WriteOnly,
    Disabled,
}

impl CacheMode {
    const ALL: [CacheMode; 4] = [
        CacheMode::Read,
        CacheMode::Write,
        CacheMode::WriteOnly,
        CacheMode::Disabled,
    ];

    /// The `cache-mode` value.
    pub fn name(self) -> &'static str {
        match self {
            CacheMode::Read => "read",
            CacheMode::Write => "write",
            CacheMode::WriteOnly => "write-only",
            CacheMode::Disabled => "none",
        }
    }

    pub fn parse(s: &str) -> Option<CacheMode> {
        CacheMode::ALL.into_iter().find(|m| m.name() == s)
    }

    pub fn can_restore(self) -> bool {
        matches!(self, CacheMode::Read | CacheMode::Write)
    }

    pub fn can_save(self) -> bool {
        matches!(self, CacheMode::Write | CacheMode::WriteOnly)
    }
}

const PERMISSION_LEVELS: &[&str] = &["read", "write", "none"];

/// Values of `permissions`, `cache-mode` and `concurrency` that GitHub rejects.
fn value_errors(m: &Yaml, at: &str, errors: &mut Vec<String>) {
    match m.get("permissions") {
        Some(Yaml::String(s)) if !["read-all", "write-all"].contains(&s.as_str()) => errors.push(
            format!("{at}.permissions: expected read-all, write-all or a mapping, got '{s}'"),
        ),
        Some(Yaml::Mapping(p)) => {
            for (scope, level) in p {
                let level = yaml_scalar_to_string(level);
                if !PERMISSION_LEVELS.contains(&level.as_str()) {
                    errors.push(format!(
                        "{at}.permissions.{}: expected read, write or none, got '{level}'",
                        yaml_scalar_to_string(scope)
                    ));
                }
            }
        }
        _ => {}
    }
    if let Some(Yaml::String(mode)) = m.get("cache-mode") {
        if CacheMode::parse(mode).is_none() && !mode.contains("${{") {
            errors.push(format!(
                "{at}.cache-mode: expected read, write, write-only or none, got '{mode}'"
            ));
        }
    }
    if let Some(c @ Yaml::Mapping(_)) = m.get("concurrency") {
        if c.get("group").is_none() {
            errors.push(format!("{at}.concurrency: missing 'group'"));
        }
        let queue = c.get("queue").map(yaml_scalar_to_string);
        if let Some(q) = queue.as_deref().filter(|q| !["single", "max"].contains(q)) {
            errors.push(format!(
                "{at}.concurrency.queue: expected single or max, got '{q}'"
            ));
        }
        let cancels = c
            .get("cancel-in-progress")
            .is_some_and(|v| !matches!(v, Yaml::Bool(false)));
        if queue.as_deref() == Some("max") && cancels {
            errors.push(format!(
                "{at}.concurrency: queue: max cannot be combined with cancel-in-progress"
            ));
        }
    }
}

/// Values of `m`'s keys that do not have the shape `types` gives them.
fn type_errors(m: &Yaml, types: &[(&str, Kind)], at: &str, errors: &mut Vec<String>) {
    for (key, kind) in types {
        if let Some(v) = m.get(*key).filter(|v| !matches!(v, Yaml::Null)) {
            if !kind_ok(v, *kind) {
                errors.push(format!(
                    "{at}.{key}: unexpected value {}",
                    yaml_scalar_to_string(v).lines().next().unwrap_or("")
                ));
            }
        }
    }
}

/// Call `f` with each step of a job's `steps` and its location, each step
/// before the steps of its `parallel` group; `in_parallel` for those.
fn for_each_step(steps: &Yaml, at: &str, f: &mut impl FnMut(&Yaml, &str, bool)) {
    let Yaml::Sequence(steps) = steps else { return };
    for (i, step) in steps.iter().enumerate() {
        visit_step(step, &format!("{at}.steps[{i}]"), false, f);
    }
}

fn visit_step(step: &Yaml, at: &str, in_parallel: bool, f: &mut impl FnMut(&Yaml, &str, bool)) {
    f(step, at, in_parallel);
    if let Some(Yaml::Sequence(group)) = step.get("parallel") {
        for (j, child) in group.iter().enumerate() {
            visit_step(child, &format!("{at}.parallel[{j}]"), true, f);
        }
    }
}

/// Errors in one step; `in_parallel` for the steps of a `parallel` group.
fn step_errors(step: &Yaml, at: &str, in_parallel: bool, errors: &mut Vec<String>) {
    if !matches!(step, Yaml::Mapping(_)) {
        errors.push(format!("{at}: a step must be a mapping"));
        return;
    }
    type_errors(step, STEP_TYPES, at, errors);
    let kinds: Vec<&str> = ["run", "uses", "wait", "wait-all", "cancel", "parallel"]
        .into_iter()
        .filter(|k| step.get(*k).is_some())
        .collect();
    match kinds.as_slice() {
        [] => errors.push(format!("{at}: needs 'run' or 'uses'")),
        ["run", "uses"] => errors.push(format!("{at}: has both 'run' and 'uses'")),
        [_] => {}
        many => errors.push(format!("{at}: has more than one of {}", many.join(", "))),
    }
    let control = kinds
        .iter()
        .any(|k| matches!(*k, "wait" | "wait-all" | "cancel" | "parallel"));
    if control && in_parallel {
        errors.push(format!(
            "{at}: a parallel group holds only 'run' and 'uses' steps"
        ));
    }
    if kinds
        .iter()
        .any(|k| matches!(*k, "wait" | "wait-all" | "cancel"))
        && step.get("if").is_some()
    {
        errors.push(format!(
            "{at}: wait, wait-all and cancel steps always run and take no 'if'"
        ));
    }
}

/// Schema errors checked on every load: required keys and value types (unknown
/// keys are left to `--strict`).
pub fn schema_errors(doc: &Yaml) -> Vec<String> {
    let mut errors = Vec::new();
    let Yaml::Mapping(_) = doc else {
        return vec!["a workflow must be a mapping".into()];
    };
    let has_on = doc.as_mapping().is_some_and(|m| {
        m.contains_key("on") || m.contains_key("true") || m.contains_key(Yaml::Bool(true))
    });
    if !has_on {
        errors.push("workflow: missing 'on'".into());
    }
    let jobs = match doc.get("jobs") {
        Some(Yaml::Mapping(j)) if !j.is_empty() => j,
        Some(Yaml::Mapping(_)) => {
            errors.push("workflow: 'jobs' is empty".into());
            return errors;
        }
        Some(_) => {
            errors.push("workflow: 'jobs' must be a mapping".into());
            return errors;
        }
        None => {
            errors.push("workflow: missing 'jobs'".into());
            return errors;
        }
    };
    type_errors(doc, WORKFLOW_TYPES, "workflow", &mut errors);
    value_errors(doc, "workflow", &mut errors);
    for (id, job) in jobs {
        let id = yaml_scalar_to_string(id);
        let at = format!("jobs.{id}");
        let valid_id = id.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_id {
            errors.push(format!(
                "{at}: job IDs must start with a letter or '_' and contain only letters, digits, '-' and '_'"
            ));
        }
        if !matches!(job, Yaml::Mapping(_)) {
            errors.push(format!("{at}: a job must be a mapping"));
            continue;
        }
        type_errors(job, JOB_TYPES, &at, &mut errors);
        let reusable = job.get("uses").is_some();
        if !reusable && job.get("runs-on").is_none() {
            errors.push(format!("{at}: missing 'runs-on'"));
        }
        if !reusable && job.get("steps").is_none() {
            errors.push(format!("{at}: missing 'steps'"));
        }
        value_errors(job, &at, &mut errors);
        if let Some(steps) = job.get("steps") {
            for_each_step(steps, &at, &mut |step, at, in_parallel| {
                step_errors(step, at, in_parallel, &mut errors)
            });
        }
    }
    errors
}

/// Reject keys outside the workflow syntax (`--strict`).
pub fn strict_check(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let doc: Yaml = serde_yaml::from_str(&text)?;
    let mut errors = Vec::new();
    let unknown = |m: &Yaml, allowed: &[(&str, Kind)], at: &str, errors: &mut Vec<String>| {
        if let Yaml::Mapping(m) = m {
            for k in m.keys() {
                let k = yaml_scalar_to_string(k);
                if !allowed.iter().any(|(key, _)| *key == k) {
                    errors.push(format!("{at}: unknown key '{k}'"));
                }
            }
        }
    };
    unknown(&doc, WORKFLOW_TYPES, "workflow", &mut errors);
    if let Some(Yaml::Mapping(jobs)) = doc.get("jobs") {
        for (id, job) in jobs {
            let at = format!("jobs.{}", yaml_scalar_to_string(id));
            unknown(job, JOB_TYPES, &at, &mut errors);
            if let Some(steps) = job.get("steps") {
                for_each_step(steps, &at, &mut |step, at, _| {
                    unknown(step, STEP_TYPES, at, &mut errors)
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}:\n  {}", path.display(), errors.join("\n  "))
    }
}

pub fn yaml_scalar_to_string(v: &Yaml) -> String {
    match v {
        Yaml::Null => String::new(),
        Yaml::Bool(b) => b.to_string(),
        // A float prints as an expression's number does, so `2.0` and
        // `${{ 2.0 }}` both give `2`.
        Yaml::Number(n) => match n
            .as_f64()
            .filter(|_| n.is_f64())
            .and_then(serde_json::Number::from_f64)
        {
            Some(f) => crate::expr::to_string(&serde_json::Value::Number(f)),
            None => n.to_string(),
        },
        Yaml::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim_end()
            .to_string(),
    }
}

pub fn yaml_to_json(v: &Yaml) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Yaml::Null => J::Null,
        Yaml::Bool(b) => J::Bool(*b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                J::from(i)
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(J::Number)
                    .unwrap_or(J::Null)
            } else {
                J::String(n.to_string())
            }
        }
        Yaml::String(s) => J::String(s.clone()),
        Yaml::Sequence(seq) => J::Array(seq.iter().map(yaml_to_json).collect()),
        Yaml::Mapping(m) => J::Object(
            m.iter()
                .map(|(k, v)| (yaml_scalar_to_string(k), yaml_to_json(v)))
                .collect(),
        ),
        Yaml::Tagged(t) => yaml_to_json(&t.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(yaml: &str) -> Vec<String> {
        schema_errors(&serde_yaml::from_str(yaml).unwrap())
    }

    #[test]
    fn background_step_kinds() {
        let ok = errors(
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - {id: s, background: true, run: sleep 1}\n      - wait: s\n      - wait-all:\n      - cancel: s\n      - parallel: [{run: a}, {uses: ./b}]\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        let bad = errors(
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - {wait: s, run: x}\n      - {wait-all: null, if: always()}\n      - parallel: [{wait: s}]\n",
        );
        assert_eq!(bad.len(), 3, "{bad:?}");
    }

    #[test]
    fn permissions_cache_mode_and_concurrency_values() {
        let ok = errors(
            "on: push\npermissions: read-all\ncache-mode: write-only\nconcurrency: {group: g, queue: max}\njobs:\n  a:\n    runs-on: x\n    permissions: {contents: read, id-token: write}\n    snapshot: img\n    steps: [{run: a}]\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        let bad = errors(
            "on: push\npermissions: all\ncache-mode: sometimes\nconcurrency: {group: g, queue: max, cancel-in-progress: true}\njobs:\n  a:\n    runs-on: x\n    permissions: {contents: admin}\n    concurrency: {cancel-in-progress: true}\n    steps: [{run: a}]\n",
        );
        assert_eq!(bad.len(), 5, "{bad:?}");
    }

    #[test]
    fn scalar_numbers_print_like_expression_numbers() {
        let s = |y: &str| yaml_scalar_to_string(&serde_yaml::from_str(y).unwrap());
        assert_eq!(s("2.0"), "2");
        assert_eq!(s("1e3"), "1000");
        assert_eq!(s("1.10"), "1.1");
        assert_eq!(s("-0.5"), "-0.5");
        assert_eq!(s("18446744073709551615"), "18446744073709551615");
        assert_eq!(s(".inf"), ".inf");
    }

    #[test]
    fn strict_checks_parallel_children() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.yml");
        std::fs::write(
            &path,
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - parallel: [{run: a, shel: bash}]\n",
        )
        .unwrap();
        let err = strict_check(&path).unwrap_err().to_string();
        assert!(
            err.contains("jobs.a.steps[0].parallel[0]") && err.contains("shel"),
            "{err}"
        );
    }
}
