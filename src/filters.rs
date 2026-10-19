//! `on.<event>` filters: `branches`, `tags`, `paths` (and their `-ignore` forms),
//! `types`, and `workflow_run.workflows`, evaluated against the event payload the
//! way GitHub decides whether a workflow is triggered.

use crate::context::RepoInfo;
use crate::workflow::yaml_scalar_to_string;
use regex::Regex;
use serde_json::Value;
use serde_yaml::Value as Yaml;

/// Why a workflow that lists `event` is not triggered by it, or `None` if it is.
pub fn skip_reason(on: &Yaml, event: &str, payload: &Value, repo: &RepoInfo) -> Option<String> {
    let filters = match on.get(event) {
        Some(f @ Yaml::Mapping(_)) => f,
        _ => return types_reason(&Yaml::Null, event, payload),
    };
    let list = |key: &str| -> Option<Vec<String>> {
        match filters.get(key)? {
            Yaml::Sequence(s) => Some(s.iter().map(yaml_scalar_to_string).collect()),
            Yaml::Null => None,
            v => Some(vec![yaml_scalar_to_string(v)]),
        }
    };
    let str_at = |p: &str| {
        payload
            .pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    if let Some(r) = types_reason(filters, event, payload) {
        return Some(r);
    }
    match event {
        "push" => {
            let git_ref = str_at("/ref").unwrap_or(&repo.git_ref);
            let (branches, tags) = (
                (list("branches"), list("branches-ignore")),
                (list("tags"), list("tags-ignore")),
            );
            let has =
                |f: &(Option<Vec<String>>, Option<Vec<String>>)| f.0.is_some() || f.1.is_some();
            if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
                if has(&tags) {
                    return ref_reason(&tags, tag, "tags");
                }
                // Path filters are not evaluated for tag pushes.
                return has(&branches).then(|| "a tag push and only branch filters".into());
            }
            let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
            if has(&branches) {
                if let Some(r) = ref_reason(&branches, branch, "branches") {
                    return Some(r);
                }
            } else if has(&tags) {
                return Some("a branch push and only tag filters".into());
            }
            paths_reason(&list("paths"), &list("paths-ignore"), || {
                push_changed_files(payload, repo)
            })
        }
        "pull_request" | "pull_request_target" => {
            let base = str_at("/pull_request/base/ref")
                .or_else(|| str_at("/repository/default_branch"))
                .unwrap_or(&repo.default_branch);
            let base = base.strip_prefix("refs/heads/").unwrap_or(base);
            if let Some(r) = ref_reason(
                &(list("branches"), list("branches-ignore")),
                base,
                "branches",
            ) {
                return Some(r);
            }
            paths_reason(&list("paths"), &list("paths-ignore"), || {
                let base = str_at("/pull_request/base/sha")?;
                let head = str_at("/pull_request/head/sha")?;
                git_diff(repo, &format!("{base}...{head}"))
            })
        }
        "merge_group" => {
            let base = str_at("/merge_group/base_ref")?;
            let base = base.strip_prefix("refs/heads/").unwrap_or(base);
            ref_reason(
                &(list("branches"), list("branches-ignore")),
                base,
                "branches",
            )
        }
        "workflow_run" => {
            if let (Some(wfs), Some(name)) = (list("workflows"), str_at("/workflow_run/name")) {
                if !wfs.iter().any(|w| glob_match(w, name)) {
                    return Some(format!("workflow '{name}' is not in workflows"));
                }
            }
            let branch = str_at("/workflow_run/head_branch")?;
            ref_reason(
                &(list("branches"), list("branches-ignore")),
                branch,
                "branches",
            )
        }
        _ => None,
    }
}

/// `types`: the payload's `action` must be listed. `pull_request` and
/// `pull_request_target` default to `opened`, `synchronize` and `reopened`.
/// A payload without `action` (a local run without `-e`) passes.
fn types_reason(filters: &Yaml, event: &str, payload: &Value) -> Option<String> {
    let action = payload.get("action").and_then(Value::as_str)?;
    let types: Vec<String> = match filters.get("types") {
        Some(Yaml::Sequence(s)) => s.iter().map(yaml_scalar_to_string).collect(),
        Some(v @ Yaml::String(_)) => vec![yaml_scalar_to_string(v)],
        _ if matches!(event, "pull_request" | "pull_request_target") => {
            ["opened", "synchronize", "reopened"]
                .map(String::from)
                .to_vec()
        }
        _ => return None,
    };
    (!types.iter().any(|t| t == action))
        .then(|| format!("activity type '{action}' is not in types"))
}

/// A ref filter pair (`branches` + `branches-ignore`, or `tags` + `tags-ignore`).
fn ref_reason(
    (include, ignore): &(Option<Vec<String>>, Option<Vec<String>>),
    name: &str,
    key: &str,
) -> Option<String> {
    if let Some(patterns) = include {
        if !patterns_match(patterns, name) {
            return Some(format!("'{name}' does not match {key}"));
        }
    }
    if let Some(patterns) = ignore {
        if patterns.iter().any(|p| glob_match(p, name)) {
            return Some(format!("'{name}' matches {key}-ignore"));
        }
    }
    None
}

/// `paths`: some changed file must match; `paths-ignore`: some changed file must
/// not. When the changed files cannot be determined, the filters pass.
fn paths_reason(
    include: &Option<Vec<String>>,
    ignore: &Option<Vec<String>>,
    changed: impl FnOnce() -> Option<Vec<String>>,
) -> Option<String> {
    if include.is_none() && ignore.is_none() {
        return None;
    }
    let files = changed()?;
    // Compiled once here, not once per changed file.
    if let Some(patterns) = include {
        let filter = Filter::new(patterns);
        if !files.iter().any(|f| filter.matches(f)) {
            return Some("no changed file matches paths".into());
        }
    }
    if let Some(patterns) = ignore {
        let ignored: Vec<Regex> = patterns.iter().filter_map(|p| glob_regex(p)).collect();
        if files.iter().all(|f| ignored.iter().any(|r| r.is_match(f))) {
            return Some("every changed file matches paths-ignore".into());
        }
    }
    None
}

/// Files a push changed: the payload's `commits` (when they list files), else
/// `git diff before after`.
fn push_changed_files(payload: &Value, repo: &RepoInfo) -> Option<Vec<String>> {
    const KEYS: [&str; 3] = ["added", "removed", "modified"];
    if let Some(commits) = payload
        .get("commits")
        .and_then(Value::as_array)
        .filter(|c| c.iter().any(|c| KEYS.iter().any(|k| c.get(k).is_some())))
    {
        let mut files: Vec<String> = commits
            .iter()
            .flat_map(|c| KEYS.map(|k| c.get(k)))
            .flatten()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|f| f.as_str().map(String::from))
            .collect();
        files.sort();
        files.dedup();
        return Some(files);
    }
    let sha = |k: &str| {
        payload
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && !s.bytes().all(|b| b == b'0'))
    };
    git_diff(repo, &format!("{}..{}", sha("before")?, sha("after")?))
}

/// The files changed in `range`; an empty diff is `Some` with no files.
fn git_diff(repo: &RepoInfo, range: &str) -> Option<Vec<String>> {
    let out = crate::context::git_output(&repo.root, &["diff", "--name-only", range])?;
    Some(out.lines().map(String::from).collect())
}

/// Patterns with `!` negations, evaluated in order: the last match decides.
fn patterns_match(patterns: &[String], name: &str) -> bool {
    Filter::new(patterns).matches(name)
}

/// Compiled patterns with `!` negations: each one negated or not, and its regex
/// (`None` for a pattern that cannot match anything).
struct Filter(Vec<(bool, Option<Regex>)>);

impl Filter {
    fn new(patterns: &[String]) -> Filter {
        Filter(
            patterns
                .iter()
                .map(|p| match p.strip_prefix('!') {
                    Some(neg) => (true, glob_regex(neg)),
                    None => (false, glob_regex(p)),
                })
                .collect(),
        )
    }

    fn matches(&self, name: &str) -> bool {
        let mut matched = false;
        for (negated, re) in &self.0 {
            if re.as_ref().is_some_and(|r| r.is_match(name)) {
                matched = !negated;
            }
        }
        matched
    }
}

/// GitHub's filter pattern syntax: `*` (no `/`), `**` (`**/` also matches no
/// directory), `?` and `+` (zero-or-one
/// and one-or-more of the preceding character), `[...]`, and `\` escapes.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    glob_regex(pattern).is_some_and(|r| r.is_match(name))
}

/// `pattern` as an anchored regex; `None` when it is not a valid one.
fn glob_regex(pattern: &str) -> Option<Regex> {
    #[cfg(test)]
    tests::COMPILED.with(|c| c.set(c.get() + 1));
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' | '+' => re.push(c),
            '[' => {
                re.push('[');
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    if c == '\\' || c == '[' {
                        re.push('\\');
                    }
                    re.push(c);
                }
                re.push(']');
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    re.push_str(&regex::escape(&next.to_string()));
                }
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn repo() -> RepoInfo {
        RepoInfo {
            root: PathBuf::from("."),
            sha: "0".repeat(40),
            git_ref: "refs/heads/feature/x".into(),
            ref_name: "feature/x".into(),
            repository: "o/r".into(),
            owner: "o".into(),
            actor: "a".into(),
            default_branch: "main".into(),
            host: "github.com".into(),
        }
    }

    fn on(yaml: &str) -> Yaml {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn globs() {
        assert!(glob_match("feature/*", "feature/x"));
        assert!(!glob_match("feature/*", "feature/x/y"));
        assert!(glob_match("feature/**", "feature/x/y"));
        assert!(glob_match("*.js", "app.js"));
        assert!(!glob_match("*.js", "src/app.js"));
        assert!(glob_match("**.js", "src/app.js"));
        assert!(glob_match("v[12].[0-9]+.[0-9]+", "v1.10.3"));
        assert!(glob_match("*ab?c", "xac"));
        assert!(glob_match("\\*x", "*x"));
        assert!(!glob_match("main", "mainline"));
        // `**/` also matches no directory (GitHub's filter cheat sheet).
        assert!(glob_match("**/README.md", "README.md"));
        assert!(glob_match("**/README.md", "a/b/README.md"));
        assert!(glob_match("docs/**/*.md", "docs/a.md"));
        assert!(glob_match("docs/**/*.md", "docs/x/y/a.md"));
        assert!(!glob_match("docs/**/*.md", "docsa.md"));
    }

    thread_local! {
        /// Patterns compiled on this thread.
        pub static COMPILED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn path_patterns_compile_once_per_run() {
        let files: Vec<String> = (0..1000).map(|n| format!("src/f{n}.rs")).collect();
        let include = Some(["docs/**", "!docs/x", "*.md"].map(String::from).to_vec());
        let ignore = Some(["src/**"].map(String::from).to_vec());
        let before = COMPILED.with(|c| c.get());
        let reason = paths_reason(&include, &None, || Some(files.clone()));
        assert_eq!(reason.as_deref(), Some("no changed file matches paths"));
        let reason = paths_reason(&None, &ignore, || Some(files.clone()));
        assert_eq!(
            reason.as_deref(),
            Some("every changed file matches paths-ignore")
        );
        assert_eq!(COMPILED.with(|c| c.get()) - before, 4);
    }

    #[test]
    fn negation_order() {
        let p = ["releases/**", "!releases/**-alpha"].map(String::from);
        assert!(patterns_match(&p, "releases/10"));
        assert!(!patterns_match(&p, "releases/10-alpha"));
    }

    #[test]
    fn push_branches_and_tags() {
        let wf = on("push: {branches: [main]}");
        assert!(skip_reason(&wf, "push", &json!({}), &repo()).is_some());
        assert!(skip_reason(&wf, "push", &json!({"ref": "refs/heads/main"}), &repo()).is_none());
        assert!(skip_reason(&wf, "push", &json!({"ref": "refs/tags/v1"}), &repo()).is_some());
        let ignore = on("push: {branches-ignore: ['feature/**']}");
        assert!(skip_reason(&ignore, "push", &json!({}), &repo()).is_some());
        let tags = on("push: {tags: ['v*']}");
        assert!(skip_reason(&tags, "push", &json!({"ref": "refs/tags/v1"}), &repo()).is_none());
        assert!(skip_reason(&tags, "push", &json!({}), &repo()).is_some());
        assert!(skip_reason(&on("push:"), "push", &json!({}), &repo()).is_none());
        assert!(skip_reason(&on("[push]"), "push", &json!({}), &repo()).is_none());
    }

    #[test]
    fn push_paths_from_commits() {
        let wf = on("push: {paths: ['src/**'], paths-ignore: []}");
        let ev = |f: &str| json!({"commits": [{"added": [], "removed": [], "modified": [f]}]});
        assert!(skip_reason(&wf, "push", &ev("src/a.rs"), &repo()).is_none());
        assert!(skip_reason(&wf, "push", &ev("docs/a.md"), &repo()).is_some());
        let ignore = on("push: {paths-ignore: ['docs/**']}");
        assert!(skip_reason(&ignore, "push", &ev("docs/a.md"), &repo()).is_some());
        // Unknown changes: the filter passes.
        assert!(skip_reason(&wf, "push", &json!({}), &repo()).is_none());
        let bare = json!({"commits": [{"id": "x", "message": "m"}]});
        assert!(skip_reason(&wf, "push", &bare, &repo()).is_none());
        assert!(skip_reason(&ignore, "push", &bare, &repo()).is_none());
    }

    #[test]
    fn pull_request_types_and_base() {
        let wf = on("pull_request: {branches: [main]}");
        assert!(skip_reason(&wf, "pull_request", &json!({}), &repo()).is_none());
        let closed = json!({"action": "closed", "pull_request": {"base": {"ref": "main"}}});
        assert!(skip_reason(&wf, "pull_request", &closed, &repo()).is_some());
        let types = on("pull_request: {types: [closed]}");
        assert!(skip_reason(&types, "pull_request", &closed, &repo()).is_none());
        let dev = json!({"action": "opened", "pull_request": {"base": {"ref": "dev"}}});
        assert!(skip_reason(&wf, "pull_request", &dev, &repo()).is_some());
        let issues = on("issues: {types: [opened]}");
        assert!(skip_reason(&issues, "issues", &json!({"action": "closed"}), &repo()).is_some());
    }

    #[test]
    fn workflow_run_names() {
        let wf = on("workflow_run: {workflows: [Build], types: [completed]}");
        let ev = |name: &str| json!({"action": "completed", "workflow_run": {"name": name}});
        assert!(skip_reason(&wf, "workflow_run", &ev("Build"), &repo()).is_none());
        assert!(skip_reason(&wf, "workflow_run", &ev("Lint"), &repo()).is_some());
    }
}
