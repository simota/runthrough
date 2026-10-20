//! Informational output: man page, option list, bug report, job graph, the
//! repository fingerprint `--watch` polls, and the pre-push hook `--install-hook` writes.

use crate::backend::Engine;
use crate::workflow::Workflow;
use clap::CommandFactory;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

fn roff_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('-', "\\-")
}

fn arg_names(a: &clap::Arg) -> String {
    let mut names = Vec::new();
    if let Some(s) = a.get_short() {
        names.push(format!("-{s}"));
    }
    if let Some(l) = a.get_long() {
        names.push(format!("--{l}"));
    }
    if names.is_empty() {
        names.push(format!("<{}>", a.get_id().as_str().to_uppercase()));
    }
    names.join(", ")
}

pub fn man_page() -> String {
    let cmd = crate::Cli::command();
    let mut out = String::new();
    out.push_str(&format!(
        ".TH RUNTHROUGH 1 \"\" \"runthrough {}\"\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(".SH NAME\nrunthrough \\- ");
    out.push_str(&roff_escape(
        &cmd.get_about().map(|a| a.to_string()).unwrap_or_default(),
    ));
    out.push_str("\n.SH SYNOPSIS\n.B runthrough\n[\\fIOPTIONS\\fR] [\\fIEVENT\\fR]\n.SH OPTIONS\n");
    for a in cmd.get_arguments() {
        out.push_str(&format!(".TP\n\\fB{}\\fR\n", roff_escape(&arg_names(a))));
        let help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
        out.push_str(&roff_escape(&help));
        let defaults: Vec<String> = a
            .get_default_values()
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        if !defaults.is_empty() {
            out.push_str(&format!(
                " (default: {})",
                roff_escape(&defaults.join(", "))
            ));
        }
        out.push('\n');
    }
    out
}

pub fn list_options() -> String {
    let cmd = crate::Cli::command();
    let opts: Vec<serde_json::Value> = cmd
        .get_arguments()
        .filter(|a| a.get_long().is_some())
        .map(|a| {
            let kind = match a.get_action() {
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => "bool",
                clap::ArgAction::Append => "stringArray",
                _ => "string",
            };
            serde_json::json!({
                "name": a.get_long(),
                "shorthand": a.get_short().map(String::from),
                "type": kind,
                "default": a.get_default_values().iter()
                    .map(|v| v.to_string_lossy().into_owned()).collect::<Vec<_>>().join(","),
                "description": a.get_help().map(|h| h.to_string()).unwrap_or_default(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&opts).unwrap_or_default()
}

fn first_line(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_else(|| "not found".into())
}

pub fn bug_report(engine: &Engine, cache: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "runthrough version: {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!(
        "OS: {} / arch: {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!(
        "CPUs: {}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    ));
    out.push_str(&format!("selected engine: {engine:?}\n"));
    out.push_str(&format!(
        "docker: {}\n",
        first_line("docker", &["version", "--format", "{{.Server.Version}}"])
    ));
    out.push_str(&format!(
        "container: {}\n",
        first_line("container", &["--version"])
    ));
    out.push_str(&format!("git: {}\n", first_line("git", &["--version"])));
    out.push_str(&format!("node: {}\n", first_line("node", &["--version"])));
    out.push_str(&format!("cache directory: {}\n", cache.display()));
    out.push_str(&format!(
        "default image: {}\n",
        crate::runner::DEFAULT_IMAGE
    ));
    out
}

/// The pre-push hook: rehearse the commit being pushed, and block the push
/// when a job fails or runthrough cannot run, saying how to push anyway.
const PRE_PUSH: &str = r#"#!/bin/sh
# Installed by `runthrough --install-hook`: every `git push` first runs the
# workflows its `push` triggers, on the HEAD commit. Add flags (e.g. --host)
# to the runthrough line below; skip the hook once with `git push --no-verify`.
head=$(git rev-parse HEAD 2>/dev/null)
while read -r local_ref local_sha remote_ref remote_sha; do
  if [ "$local_sha" != "$head" ] && [ "$local_sha" != 0000000000000000000000000000000000000000 ]; then
    echo "runthrough: rehearsing HEAD ($head); $local_ref is not checked out and is not rehearsed" >&2
  fi
done
runthrough --committed --quiet push
status=$?
if [ "$status" -eq 1 ]; then
  echo "runthrough: a job failed, so the push was stopped (push anyway: git push --no-verify)" >&2
elif [ "$status" -ne 0 ]; then
  echo "runthrough could not run the workflows (exit $status), so the push was stopped (push anyway: git push --no-verify)" >&2
fi
exit "$status"
"#;

/// Write the pre-push hook into the hooks directory git uses for `root`
/// (`core.hooksPath` included); an existing hook is never replaced.
pub fn install_hook(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks/pre-push"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "{} is not a git repository: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let hook = root.join(String::from_utf8_lossy(&out.stdout).trim());
    if hook.exists() {
        anyhow::bail!(
            "{} already exists; add `runthrough --committed --quiet push` to it, or remove it and run --install-hook again",
            hook.display()
        );
    }
    if let Some(dir) = hook.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&hook, PRE_PUSH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(hook)
}

/// Box-and-arrow drawing of a workflow's stages.
pub fn graph(wf: &Workflow, stages: &[Vec<String>]) -> String {
    let mut out = format!("{} ({})\n", wf.display_name(), wf.path.display());
    for (i, stage) in stages.iter().enumerate() {
        let boxes: Vec<String> = stage
            .iter()
            .map(|id| {
                let name = wf.jobs[id].name.clone().unwrap_or_else(|| id.clone());
                if name == *id {
                    id.clone()
                } else {
                    format!("{id}: {name}")
                }
            })
            .collect();
        let top: Vec<String> = boxes
            .iter()
            .map(|b| format!("╭{}╮", "─".repeat(b.chars().count() + 2)))
            .collect();
        let mid: Vec<String> = boxes.iter().map(|b| format!("│ {b} │")).collect();
        let bot: Vec<String> = boxes
            .iter()
            .map(|b| format!("╰{}╯", "─".repeat(b.chars().count() + 2)))
            .collect();
        out.push_str(&format!(
            "{}\n{}\n{}\n",
            top.join(" "),
            mid.join(" "),
            bot.join(" ")
        ));
        if i + 1 < stages.len() {
            out.push_str("    ⬇\n");
        }
    }
    out
}

/// Hash of the paths, sizes and modification times of the repository's
/// files, tracked and untracked, without ignored ones.
pub fn fingerprint(root: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    // Unlike the checkout copy (`--use-gitignore`), ignored files are always left out.
    let files = crate::actions::list_repo_files(root, true);
    for f in files {
        let f = root.join(f);
        f.hash(&mut h);
        if let Ok(m) = std::fs::metadata(&f) {
            m.len().hash(&mut h);
            m.modified().ok().hash(&mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_hook_writes_once() {
        let repo = tempfile::tempdir().unwrap();
        assert!(install_hook(repo.path()).is_err(), "not a repository");
        assert!(Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        let hook = install_hook(repo.path()).unwrap();
        assert_eq!(hook, repo.path().join(".git/hooks/pre-push"));
        let script = std::fs::read_to_string(&hook).unwrap();
        assert!(script.contains("runthrough --committed --quiet push"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&hook).unwrap().permissions().mode() & 0o111,
                0o111
            );
        }
        std::fs::write(&hook, "mine").unwrap();
        let err = install_hook(repo.path()).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), "mine");
    }
}
