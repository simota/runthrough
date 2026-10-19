//! Runner protocol: environment files (`GITHUB_OUTPUT`, `GITHUB_ENV`, ...) and
//! stdout workflow commands (`::set-output`, `::add-mask::`, `::group::`, ...).

use anyhow::{bail, Result};
use indexmap::IndexMap;

/// Parse a `GITHUB_OUTPUT` / `GITHUB_ENV` / `GITHUB_STATE` file:
/// `name=value` lines and `name<<DELIM` heredocs.
pub fn parse_env_file(text: &str) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let heredoc = line.find("<<");
        let eq = line.find('=');
        match (heredoc, eq) {
            (Some(h), e) if e.is_none_or(|e| h < e) => {
                let name = line[..h].to_string();
                let delim = &line[h + 2..];
                let mut body: Vec<&str> = Vec::new();
                loop {
                    match lines.next() {
                        Some(l) if l == delim => break,
                        Some(l) => body.push(l),
                        None => bail!("matching delimiter '{delim}' not found for '{name}'"),
                    }
                }
                out.insert(name, body.join("\n"));
            }
            (_, Some(e)) => {
                out.insert(line[..e].to_string(), line[e + 1..].to_string());
            }
            _ => bail!("invalid line in environment file: {line}"),
        }
    }
    Ok(out)
}

#[derive(Debug, PartialEq)]
pub enum Command {
    SetOutput(String, String),
    SaveState(String, String),
    AddMask(String),
    AddPath(String),
    SetEnv(String, String),
    Group(String),
    EndGroup,
    Annotation {
        level: String,
        message: String,
    },
    Debug(String),
    StopCommands(String),
    /// `::add-matcher::path`: a problem matcher file.
    AddMatcher(String),
    /// `::remove-matcher owner=x::`.
    RemoveMatcher(String),
}

/// Recognise a `::command key=val,...::message` line.
pub fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim_start();
    // `::name k=v,k=v::message`, or Azure DevOps style `##[name k=v;k=v]message`.
    let (head, msg, sep) = if let Some(rest) = line.strip_prefix("::") {
        let end = rest.find("::")?;
        (&rest[..end], unescape_data(&rest[end + 2..]), ',')
    } else {
        let rest = line.strip_prefix("##[")?;
        let end = rest.find(']')?;
        (&rest[..end], unescape_data(&rest[end + 1..]), ';')
    };
    let (name, props) = match head.split_once(' ') {
        Some((n, p)) => (n, p),
        None => (head, ""),
    };
    let prop = |key: &str| -> String {
        props
            .split(sep)
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| unescape_prop(v))
            .unwrap_or_default()
    };
    Some(match name {
        "set-output" => Command::SetOutput(prop("name"), msg),
        "save-state" => Command::SaveState(prop("name"), msg),
        "set-env" => Command::SetEnv(prop("name"), msg),
        "add-path" => Command::AddPath(msg),
        "add-mask" => Command::AddMask(msg),
        "group" => Command::Group(msg),
        "endgroup" => Command::EndGroup,
        "debug" => Command::Debug(msg),
        "error" | "warning" | "notice" => Command::Annotation {
            level: name.to_string(),
            message: msg,
        },
        "stop-commands" => Command::StopCommands(msg),
        "add-matcher" => Command::AddMatcher(msg),
        "remove-matcher" => Command::RemoveMatcher(prop("owner")),
        _ => return None,
    })
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

fn unescape_prop(s: &str) -> String {
    unescape_data(s).replace("%3A", ":").replace("%2C", ",")
}

/// Replace every secret / masked value with `***`.
pub fn mask(line: &str, masks: &[String]) -> String {
    let mut out = line.to_string();
    // Most lines hold no secret: only a value that occurs costs a copy.
    for m in masks {
        if !m.is_empty() && out.contains(m.as_str()) {
            out = out.replace(m.as_str(), "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file() {
        let f = parse_env_file("a=1\nmulti<<EOF\nline1\nline2\nEOF\nb=x=y\n").unwrap();
        assert_eq!(f["a"], "1");
        assert_eq!(f["multi"], "line1\nline2");
        assert_eq!(f["b"], "x=y");
        assert!(parse_env_file("x<<EOF\nno end").is_err());
    }

    #[test]
    fn workflow_commands() {
        assert_eq!(
            parse_command("::set-output name=v::1.0"),
            Some(Command::SetOutput("v".into(), "1.0".into()))
        );
        assert_eq!(
            parse_command("::add-mask::s3cr3t"),
            Some(Command::AddMask("s3cr3t".into()))
        );
        assert_eq!(parse_command("::endgroup::"), Some(Command::EndGroup));
        assert_eq!(
            parse_command("::error file=a.rs,line=1::bad%0Athing"),
            Some(Command::Annotation {
                level: "error".into(),
                message: "bad\nthing".into()
            })
        );
        assert_eq!(parse_command("plain output"), None);
        assert_eq!(
            parse_command("::add-matcher::/x/tsc.json"),
            Some(Command::AddMatcher("/x/tsc.json".into()))
        );
        assert_eq!(
            parse_command("::remove-matcher owner=tsc::"),
            Some(Command::RemoveMatcher("tsc".into()))
        );
        assert_eq!(
            parse_command("##[set-output name=az;]azure"),
            Some(Command::SetOutput("az".into(), "azure".into()))
        );
        assert_eq!(
            parse_command("##[warning file=a.rs;line=2]careful"),
            Some(Command::Annotation {
                level: "warning".into(),
                message: "careful".into()
            })
        );
    }

    #[test]
    fn masking() {
        assert_eq!(mask("token=abc123 ok", &["abc123".into()]), "token=*** ok");
        // Masks still apply in order: an earlier one can hide part of a later one.
        assert_eq!(mask("abcd bcd", &["abc".into(), "bcd".into()]), "***d ***");
        assert_eq!(mask("no secret here", &["abc".into()]), "no secret here");
    }
}
