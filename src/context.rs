//! Repository facts used to build the `github` context.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub sha: String,
    pub git_ref: String,
    pub ref_name: String,
    pub repository: String,
    pub owner: String,
    pub actor: String,
    /// From `refs/remotes/<remote>/HEAD`, else `main`.
    pub default_branch: String,
    /// GitHub host name (`github.com`, or a GitHub Enterprise Server host).
    pub host: String,
}

/// `git -C root <args>`'s stdout when it succeeds.
pub(crate) fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git_output`, trimmed; `None` when empty.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    git_output(root, args)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `git@github.com:owner/repo.git` / `https://github.com/owner/repo` → `owner/repo`
pub fn repo_slug(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let tail = s.rsplit_once(':').map(|(_, t)| t).unwrap_or(s);
    let parts: Vec<&str> = tail.split('/').filter(|p| !p.is_empty()).collect();
    (parts.len() >= 2).then(|| format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]))
}

impl RepoInfo {
    pub fn detect(root: &Path, remote: &str) -> RepoInfo {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let sha = git(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "0".repeat(40));
        let git_ref = git(&root, &["symbolic-ref", "-q", "HEAD"])
            .or_else(|| {
                git(&root, &["describe", "--tags", "--exact-match"])
                    .map(|t| format!("refs/tags/{t}"))
            })
            .unwrap_or_else(|| "refs/heads/main".into());
        let ref_name = git_ref
            .rsplit_once('/')
            .map(|(_, n)| n)
            .unwrap_or(&git_ref)
            .to_string();
        let ref_name = git_ref
            .strip_prefix("refs/heads/")
            .or_else(|| git_ref.strip_prefix("refs/tags/"))
            .map(String::from)
            .unwrap_or(ref_name);
        let dir_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        let repository = git(&root, &["remote", "get-url", remote])
            .and_then(|u| repo_slug(&u))
            .unwrap_or_else(|| format!("local/{dir_name}"));
        let owner = repository.split('/').next().unwrap_or("local").to_string();
        let actor = git(&root, &["config", "user.name"]).unwrap_or_else(|| "runthrough".into());
        let default_branch = git(
            &root,
            &[
                "symbolic-ref",
                "--short",
                &format!("refs/remotes/{remote}/HEAD"),
            ],
        )
        .and_then(|r| r.split_once('/').map(|(_, b)| b.to_string()))
        .unwrap_or_else(|| "main".into());
        RepoInfo {
            root,
            sha,
            git_ref,
            ref_name,
            repository,
            owner,
            actor,
            default_branch,
            host: "github.com".into(),
        }
    }

    pub fn server_url(&self) -> String {
        format!("https://{}", self.host)
    }

    /// Fill the payload's `repository` object the way GitHub sends it, keeping
    /// fields the event file already has (actions such as gitleaks read
    /// `repository.owner.login`).
    pub fn fill_event_repository(&self, event: &mut Value, default_branch: &str) {
        if !event["repository"].is_object() {
            event["repository"] = json!({});
        }
        let name = self.repository.split_once('/').map_or("", |(_, n)| n);
        let html_url = format!("{}/{}", self.server_url(), self.repository);
        let r = &mut event["repository"];
        for (k, v) in [
            ("name", name),
            ("full_name", &self.repository),
            ("html_url", &html_url),
            ("default_branch", default_branch),
        ] {
            if r.get(k).is_none() {
                r[k] = json!(v);
            }
        }
        if !r["owner"].is_object() {
            r["owner"] = json!({});
        }
        if r["owner"].get("login").is_none() {
            r["owner"]["login"] = json!(self.owner);
        }
    }

    /// A push payload for HEAD when no event file is given: `ref`, `after`,
    /// `commits` and `head_commit` (actions such as gitleaks read `commits`).
    /// `before` is what pushing HEAD now would send (see `push_before`); without
    /// it, `on.push.paths` passes without known changes.
    pub fn push_event(&self, remote: &str) -> Option<Value> {
        let log = git(
            &self.root,
            &[
                "log",
                "-1",
                "--format=%H%x00%T%x00%cI%x00%an%x00%ae%x00%cn%x00%ce%x00%B",
            ],
        )?;
        let mut event = self.push_event_from_log(&log);
        if let Some(before) = self.push_before(remote) {
            event["before"] = json!(before);
        }
        Some(event)
    }

    /// A merge_group payload for HEAD when no event file is given: the queue
    /// would test HEAD (`head_sha`, `head_ref`) on top of the default branch,
    /// whose commit is where HEAD forked from `<remote>/<default>` (`base_sha`).
    pub fn merge_group_event(&self, remote: &str, default_branch: &str) -> Value {
        let base_sha = git(
            &self.root,
            &[
                "merge-base",
                "HEAD",
                &format!("refs/remotes/{remote}/{default_branch}"),
            ],
        )
        .unwrap_or_else(|| self.sha.clone());
        json!({
            "action": "checks_requested",
            "merge_group": {
                "head_sha": self.sha,
                "head_ref": self.git_ref,
                "base_sha": base_sha,
                "base_ref": format!("refs/heads/{default_branch}"),
            },
        })
    }

    /// A pull_request payload for HEAD when no event file is given: a PR from
    /// the current branch (`head`) into the default branch (`base`), so
    /// `github.base_ref`/`head_ref` are set. `base.sha` is `<remote>/<default>`
    /// unless it is HEAD itself, where `on.pull_request.paths` would see no
    /// changes. No `action`, so every `types` filter passes.
    pub fn pull_request_event(&self, remote: &str, default_branch: &str) -> Value {
        let mut event = json!({
            "pull_request": {
                "head": {"ref": self.ref_name, "sha": self.sha},
                "base": {"ref": default_branch},
            },
        });
        let base_sha = git(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/remotes/{remote}/{default_branch}^{{commit}}"),
            ],
        );
        if let Some(sha) = base_sha.filter(|s| *s != self.sha) {
            event["pull_request"]["base"]["sha"] = json!(sha);
        }
        event
    }

    /// `<remote>/<branch>` when HEAD is ahead of it, as GitHub's `before` for a
    /// push of HEAD; `None` for a new, behind or up-to-date branch.
    fn push_before(&self, remote: &str) -> Option<String> {
        let branch = self.git_ref.strip_prefix("refs/heads/")?;
        let before = git(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/remotes/{remote}/{branch}^{{commit}}"),
            ],
        )?;
        let ahead = before != self.sha
            && Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["merge-base", "--is-ancestor", &before, "HEAD"])
                .status()
                .is_ok_and(|s| s.success());
        ahead.then_some(before)
    }

    /// `log`: `git log -1` fields separated by NUL (see `push_event`).
    fn push_event_from_log(&self, log: &str) -> Value {
        let f: Vec<&str> = log.splitn(8, '\0').collect();
        let at = |i: usize| f.get(i).copied().unwrap_or_default();
        let commit = json!({
            "id": at(0),
            "tree_id": at(1),
            "distinct": true,
            "message": at(7).trim_end(),
            "timestamp": at(2),
            "url": format!("{}/{}/commit/{}", self.server_url(), self.repository, at(0)),
            "author": {"name": at(3), "email": at(4)},
            "committer": {"name": at(5), "email": at(6)},
        });
        json!({
            "ref": self.git_ref,
            "after": at(0),
            "commits": [commit],
            "head_commit": commit,
        })
    }

    pub fn api_url(&self) -> String {
        if self.host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }

    pub fn graphql_url(&self) -> String {
        if self.host == "github.com" {
            "https://api.github.com/graphql".into()
        } else {
            format!("https://{}/api/graphql", self.host)
        }
    }

    pub fn github_context(
        &self,
        event_name: &str,
        event: &Value,
        workflow: &str,
        workspace: &Path,
    ) -> Value {
        let mut ctx = json!({
            "event_name": event_name,
            "event": event,
            "sha": self.sha,
            "ref": self.git_ref,
            "ref_name": self.ref_name,
            "ref_type": if self.git_ref.starts_with("refs/tags/") { "tag" } else { "branch" },
            "repository": self.repository,
            "repository_owner": self.owner,
            "actor": self.actor,
            "triggering_actor": self.actor,
            "workflow": workflow,
            "workspace": workspace.to_string_lossy(),
            "run_id": std::process::id().to_string(),
            "run_number": "1",
            "run_attempt": "1",
            "retention_days": "0",
            "server_url": self.server_url(),
            "api_url": self.api_url(),
            "graphql_url": self.graphql_url(),
            "head_ref": "",
            "base_ref": "",
        });
        if let Some(pr) = event.get("pull_request") {
            ctx["head_ref"] = pr.pointer("/head/ref").cloned().unwrap_or(json!(""));
            ctx["base_ref"] = pr.pointer("/base/ref").cloned().unwrap_or(json!(""));
        }
        if let Some((git_ref, sha)) = event_ref_and_sha(event_name, event) {
            if let Some(r) = git_ref {
                let (ref_type, ref_name) = ref_type_and_name(&r);
                ctx["ref"] = json!(r);
                ctx["ref_type"] = json!(ref_type);
                ctx["ref_name"] = json!(ref_name);
            }
            if let Some(sha) = sha {
                ctx["sha"] = json!(sha);
            }
        }
        // The caller's GITHUB_*_URL apply unless --github-instance names a host:
        // inside a GitHub Actions job they are always set (to github.com).
        let overridable = self.host == "github.com";
        for (key, var) in [
            ("server_url", "GITHUB_SERVER_URL"),
            ("api_url", "GITHUB_API_URL"),
            ("graphql_url", "GITHUB_GRAPHQL_URL"),
        ]
        .into_iter()
        .filter(|_| overridable)
        {
            if let Ok(v) = std::env::var(var).map(|v| v.trim().to_string()) {
                if !v.is_empty() {
                    ctx[key] = json!(v);
                }
            }
        }
        ctx
    }
}

/// The ref and SHA an event implies, as GitHub (and act) derive them; `None`
/// fields fall back to the local repository.
fn event_ref_and_sha(event_name: &str, event: &Value) -> Option<(Option<String>, Option<String>)> {
    let s = |p: &str| {
        event
            .pointer(p)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let number = event.get("number").and_then(|n| {
        n.as_u64()
            .map(|n| n.to_string())
            .or_else(|| n.as_str().map(String::from))
    });
    Some(match event_name {
        "pull_request_target" => (
            s("/pull_request/base/ref").map(|r| format!("refs/heads/{r}")),
            s("/pull_request/base/sha"),
        ),
        "pull_request" | "pull_request_review" | "pull_request_review_comment" => {
            (number.map(|n| format!("refs/pull/{n}/merge")), None)
        }
        "merge_group" => (s("/merge_group/head_ref"), s("/merge_group/head_sha")),
        "deployment" | "deployment_status" => (s("/deployment/ref"), s("/deployment/sha")),
        "release" => (
            s("/release/tag_name").map(|t| format!("refs/tags/{t}")),
            None,
        ),
        "push" | "create" | "workflow_dispatch" => {
            let deleted = event.get("deleted").and_then(Value::as_bool);
            (s("/ref"), s("/after").filter(|_| deleted == Some(false)))
        }
        _ => return None,
    })
}

/// `refs/heads/x` → (`branch`, `x`), `refs/tags/x` → (`tag`, `x`), `refs/pull/7/merge` → (``, `7/merge`).
fn ref_type_and_name(r: &str) -> (&'static str, String) {
    if let Some(n) = r.strip_prefix("refs/tags/") {
        ("tag", n.to_string())
    } else if let Some(n) = r.strip_prefix("refs/heads/") {
        ("branch", n.to_string())
    } else if let Some(n) = r.strip_prefix("refs/pull/") {
        ("", n.to_string())
    } else {
        ("branch", r.to_string())
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn event_repository_is_filled_without_overriding_the_file() {
        let repo = RepoInfo {
            root: PathBuf::from("."),
            sha: String::new(),
            git_ref: String::new(),
            ref_name: String::new(),
            repository: "o/r".into(),
            owner: "o".into(),
            actor: String::new(),
            default_branch: "main".into(),
            host: "github.com".into(),
        };
        let mut event = json!({});
        repo.fill_event_repository(&mut event, "main");
        assert_eq!(
            event["repository"],
            json!({"name": "r", "full_name": "o/r", "html_url": "https://github.com/o/r",
                   "default_branch": "main", "owner": {"login": "o"}})
        );
        let mut event = json!({"repository": {"name": "x", "owner": {"id": 1}}});
        repo.fill_event_repository(&mut event, "dev");
        assert_eq!(event["repository"]["name"], "x");
        assert_eq!(event["repository"]["default_branch"], "dev");
        assert_eq!(event["repository"]["owner"], json!({"id": 1, "login": "o"}));

        let log = [
            "abc",
            "tre",
            "2026-01-02T03:04:05+09:00",
            "A",
            "a@x",
            "C",
            "c@x",
            "subject\n\nbody",
        ]
        .join("\0");
        let push = RepoInfo {
            git_ref: "refs/heads/main".into(),
            ..repo
        }
        .push_event_from_log(&log);
        assert_eq!(push["ref"], "refs/heads/main");
        assert_eq!(push["after"], "abc");
        assert!(push.get("before").is_none());
        assert_eq!(push["commits"][0], push["head_commit"]);
        assert_eq!(push["head_commit"]["message"], "subject\n\nbody");
        assert_eq!(
            push["head_commit"]["url"],
            "https://github.com/o/r/commit/abc"
        );
        assert_eq!(push["head_commit"]["committer"]["email"], "c@x");
    }

    #[test]
    fn refs_from_events() {
        let pr = json!({"number": 7, "pull_request": {"base": {"ref": "main", "sha": "b"}}});
        assert_eq!(
            event_ref_and_sha("pull_request", &pr),
            Some((Some("refs/pull/7/merge".into()), None))
        );
        assert_eq!(
            event_ref_and_sha("pull_request_target", &pr),
            Some((Some("refs/heads/main".into()), Some("b".into())))
        );
        let mg =
            json!({"merge_group": {"head_ref": "refs/heads/q", "head_sha": "h", "base_sha": "b"}});
        assert_eq!(
            event_ref_and_sha("merge_group", &mg),
            Some((Some("refs/heads/q".into()), Some("h".into())))
        );
        let push = json!({"ref": "refs/tags/v1", "after": "abc", "deleted": false});
        assert_eq!(
            event_ref_and_sha("push", &push),
            Some((Some("refs/tags/v1".into()), Some("abc".into())))
        );
        let rel = json!({"release": {"tag_name": "v2"}});
        assert_eq!(
            event_ref_and_sha("release", &rel),
            Some((Some("refs/tags/v2".into()), None))
        );
        assert_eq!(event_ref_and_sha("schedule", &json!({})), None);
        assert_eq!(
            ref_type_and_name("refs/pull/7/merge"),
            ("", "7/merge".into())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_before_is_the_remote_branch_head_is_ahead_of() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@x"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "a"]);
        let a = git(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        run(&["update-ref", "refs/remotes/origin/main", &a]);
        // Up to date with the remote: nothing would be pushed.
        let repo = RepoInfo::detect(dir.path(), "origin");
        assert_eq!(repo.push_before("origin"), None);
        run(&["commit", "-q", "--allow-empty", "-m", "b"]);
        let repo = RepoInfo::detect(dir.path(), "origin");
        assert_eq!(repo.push_before("origin").as_deref(), Some(a.as_str()));
        assert_eq!(repo.push_event("origin").unwrap()["before"], a.as_str());
        // A branch the remote does not have yet.
        run(&["checkout", "-q", "-b", "topic"]);
        let repo = RepoInfo::detect(dir.path(), "origin");
        assert_eq!(repo.push_before("origin"), None);
        // Behind the remote.
        run(&["checkout", "-q", "main"]);
        run(&["reset", "-q", "--hard", &a]);
        run(&["update-ref", "refs/remotes/origin/main", "topic"]);
        let repo = RepoInfo::detect(dir.path(), "origin");
        assert_eq!(repo.push_before("origin"), None);
    }

    #[test]
    fn slugs() {
        assert_eq!(
            repo_slug("git@github.com:nektos/act.git").as_deref(),
            Some("nektos/act")
        );
        assert_eq!(
            repo_slug("https://github.com/nektos/act").as_deref(),
            Some("nektos/act")
        );
        assert_eq!(repo_slug("nothing"), None);
    }
}
