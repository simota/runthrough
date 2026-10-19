//! Problem matchers (`::add-matcher::`): regular expressions over step output
//! that turn matching lines into annotations, as GitHub's runner does.
//!
//! Patterns are compiled with the `regex` crate, so .NET-only syntax such as
//! look-around or back-references is rejected when the matcher is added.

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;

#[derive(Deserialize)]
struct MatcherFile {
    #[serde(rename = "problemMatcher")]
    problem_matcher: Vec<MatcherDef>,
}

#[derive(Deserialize)]
struct MatcherDef {
    owner: String,
    severity: Option<String>,
    pattern: Vec<PatternDef>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PatternDef {
    regexp: String,
    file: Option<usize>,
    from_path: Option<usize>,
    line: Option<usize>,
    column: Option<usize>,
    severity: Option<usize>,
    code: Option<usize>,
    message: Option<usize>,
    #[serde(default, rename = "loop")]
    looping: bool,
}

#[derive(Clone)]
struct Pattern {
    regex: Regex,
    def: PatternDef,
}

/// What a matcher captured; later patterns of a multi-line matcher fill in more.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Issue {
    pub severity: String,
    pub file: Option<String>,
    pub line: Option<String>,
    pub column: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

impl Issue {
    /// `file:line:col: ` (as much of it as is known), for the log line.
    pub fn location(&self) -> String {
        let Some(file) = &self.file else {
            return String::new();
        };
        let mut out = file.clone();
        for part in [&self.line, &self.column].into_iter().flatten() {
            out.push(':');
            out.push_str(part);
        }
        out + ": "
    }
}

#[derive(Default, Clone)]
struct Captured {
    file: Option<String>,
    from_path: Option<String>,
    line: Option<String>,
    column: Option<String>,
    severity: Option<String>,
    code: Option<String>,
    message: Option<String>,
}

#[derive(Clone)]
pub struct ProblemMatcher {
    pub owner: String,
    severity: Option<String>,
    patterns: Vec<Pattern>,
    /// Index of the next pattern of a multi-line matcher, and what the earlier
    /// patterns captured.
    next: usize,
    captured: Captured,
}

/// Parse a problem matcher file (`{"problemMatcher": [...]}`).
pub fn parse(json: &str) -> Result<Vec<ProblemMatcher>> {
    let file: MatcherFile = serde_json::from_str(json).context("invalid problem matcher file")?;
    file.problem_matcher
        .into_iter()
        .map(|m| {
            if m.owner.is_empty() {
                bail!("a problem matcher needs an owner");
            }
            let Some(last) = m.pattern.last() else {
                bail!("problem matcher '{}' has no patterns", m.owner);
            };
            if last.message.is_none() && m.pattern.iter().all(|p| p.message.is_none()) {
                bail!("problem matcher '{}' never captures a message", m.owner);
            }
            if m.pattern.iter().rev().skip(1).any(|p| p.looping) {
                bail!(
                    "problem matcher '{}': only the last pattern may loop",
                    m.owner
                );
            }
            if last.looping && m.pattern.len() == 1 {
                bail!(
                    "problem matcher '{}': a single pattern cannot loop",
                    m.owner
                );
            }
            let patterns = m
                .pattern
                .into_iter()
                .map(|def| {
                    let regex = Regex::new(&def.regexp).with_context(|| {
                        format!("problem matcher '{}': invalid regexp", m.owner)
                    })?;
                    Ok(Pattern { regex, def })
                })
                .collect::<Result<_>>()?;
            Ok(ProblemMatcher {
                owner: m.owner,
                severity: m.severity,
                patterns,
                next: 0,
                captured: Captured::default(),
            })
        })
        .collect()
}

impl ProblemMatcher {
    /// Feed one output line; returns an issue when the last pattern matches.
    pub fn feed(&mut self, line: &str) -> Option<Issue> {
        if self.next == self.patterns.len() {
            // After a full match, a looping last pattern keeps matching lines;
            // the first line it misses starts over.
            if let Some(issue) = self.try_pattern(self.patterns.len() - 1, line) {
                return issue;
            }
            self.reset();
        }
        if let Some(issue) = self.try_pattern(self.next, line) {
            return issue;
        }
        if self.next > 0 {
            // A multi-line matcher that breaks off may start again on this line.
            self.reset();
            return self.try_pattern(0, line).flatten();
        }
        None
    }

    /// Note that a matcher is starting on a new step's output.
    pub fn restart(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.next = 0;
        self.captured = Captured::default();
    }

    /// `None` when pattern `index` does not match the line; `Some(None)` when it
    /// matched but the matcher needs more lines or captured no message.
    fn try_pattern(&mut self, index: usize, line: &str) -> Option<Option<Issue>> {
        let pattern = &self.patterns[index];
        let caps = pattern.regex.captures(line)?;
        let get = |g: Option<usize>| {
            g.and_then(|g| caps.get(g))
                .map(|m| m.as_str().to_string())
                .filter(|s| !s.is_empty())
        };
        let d = &pattern.def;
        // What the earlier patterns captured; a looping last pattern starts from
        // it again on every line it matches.
        let earlier = std::mem::take(&mut self.captured);
        let mut captured = earlier.clone();
        for (slot, group) in [
            (&mut captured.file, d.file),
            (&mut captured.from_path, d.from_path),
            (&mut captured.line, d.line),
            (&mut captured.column, d.column),
            (&mut captured.severity, d.severity),
            (&mut captured.code, d.code),
            (&mut captured.message, d.message),
        ] {
            if let Some(v) = get(group) {
                *slot = Some(v);
            }
        }
        if index + 1 < self.patterns.len() {
            self.captured = captured;
            self.next = index + 1;
            return Some(None);
        }
        if d.looping {
            self.captured = earlier;
            self.next = self.patterns.len();
        } else {
            self.reset();
        }
        let Some(message) = captured.message else {
            return Some(None);
        };
        let severity = captured
            .severity
            .or_else(|| self.severity.clone())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let severity = match severity.as_str() {
            "warning" | "notice" => severity,
            _ => "error".into(),
        };
        Some(Some(Issue {
            severity,
            file: captured.file,
            line: captured.line,
            column: captured.column,
            code: captured.code,
            message,
        }))
    }
}

/// Add `new` matchers, replacing any with the same owner (case-insensitive).
pub fn register(list: &mut Vec<ProblemMatcher>, new: Vec<ProblemMatcher>) {
    for m in new {
        list.retain(|o| !o.owner.eq_ignore_ascii_case(&m.owner));
        list.push(m);
    }
}

pub fn remove(list: &mut Vec<ProblemMatcher>, owner: &str) {
    list.retain(|o| !o.owner.eq_ignore_ascii_case(owner));
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSC: &str = r#"{"problemMatcher": [{"owner": "tsc", "pattern": [{
        "regexp": "^([^(]+)\\((\\d+),(\\d+)\\): (error|warning) (TS\\d+): (.*)$",
        "file": 1, "line": 2, "column": 3, "severity": 4, "code": 5, "message": 6}]}]}"#;

    #[test]
    fn single_line_pattern() {
        let mut m = parse(TSC).unwrap().remove(0);
        let issue = m
            .feed("src/a.ts(3,7): warning TS2304: Cannot find name 'x'.")
            .unwrap();
        assert_eq!(issue.severity, "warning");
        assert_eq!(issue.location(), "src/a.ts:3:7: ");
        assert_eq!(issue.code.as_deref(), Some("TS2304"));
        assert_eq!(issue.message, "Cannot find name 'x'.");
        assert_eq!(m.feed("plain output"), None);
    }

    #[test]
    fn multi_line_pattern_with_loop() {
        let json = r#"{"problemMatcher": [{"owner": "eslint", "severity": "warning", "pattern": [
            {"regexp": "^([^\\s].*)$", "file": 1},
            {"regexp": "^\\s+(\\d+):(\\d+)\\s+(error|warning)\\s+(.*)$",
             "line": 1, "column": 2, "severity": 3, "message": 4, "loop": true}]}]}"#;
        let mut m = parse(json).unwrap().remove(0);
        assert_eq!(m.feed("src/a.js"), None);
        let a = m.feed("  1:2  error  Missing semicolon").unwrap();
        assert_eq!(
            (a.file.as_deref(), a.severity.as_str()),
            (Some("src/a.js"), "error")
        );
        let b = m.feed("  4:1  warning  Unused x").unwrap();
        assert_eq!(
            (b.file.as_deref(), b.line.as_deref()),
            (Some("src/a.js"), Some("4"))
        );
        // A line the loop misses starts over: here it is a new file.
        assert_eq!(m.feed("src/b.js"), None);
        let c = m.feed("  9:9  error  Bad").unwrap();
        assert_eq!(c.file.as_deref(), Some("src/b.js"));
    }

    #[test]
    fn default_severity_and_owner_replacement() {
        let json = r#"{"problemMatcher": [{"owner": "x", "pattern": [{"regexp": "^E: (.*)$", "message": 1}]}]}"#;
        let mut list = parse(TSC).unwrap();
        register(&mut list, parse(json).unwrap());
        register(&mut list, parse(json).unwrap());
        assert_eq!(list.len(), 2);
        let issue = list[1].feed("E: boom").unwrap();
        assert_eq!(
            (issue.severity.as_str(), issue.location()),
            ("error", String::new())
        );
        remove(&mut list, "TSC");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn invalid_matchers_are_rejected() {
        for bad in [
            r#"{"problemMatcher": [{"owner": "x", "pattern": []}]}"#,
            r#"{"problemMatcher": [{"owner": "x", "pattern": [{"regexp": "a"}]}]}"#,
            r#"{"problemMatcher": [{"owner": "x", "pattern": [{"regexp": "(?<=a)b", "message": 0}]}]}"#,
            r#"{"problemMatcher": [{"owner": "x", "pattern": [{"regexp": "a", "message": 0, "loop": true}]}]}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
