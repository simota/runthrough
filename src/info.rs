//! Informational output: man page, option list, bug report, job graph, and the
//! repository fingerprint `--watch` polls.

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
