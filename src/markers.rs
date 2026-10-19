//! The status lines the runner logs for jobs and steps, and their parsing:
//! `history` rebuilds jobs and steps from them for the dashboard.

const JOB_START: &str = "🚀 start job on ";
const JOB_SKIPPED: &str = "⏭️  job skipped";
const STEP_START: &str = "⭐ ";
const STEP_SKIPPED: &str = "⏭️  ";
/// Prefixes of a step's end line, by its status.
const STEP_ENDS: [(&str, &str); 4] = [
    ("  ✅ ", "success"),
    ("  ❌ ", "failure"),
    ("  🛑 ", "cancelled"),
    ("  ⚠️  ", "warning"),
];

/// `🚀 start job on <backend>`.
pub fn job_start(backend: &str) -> String {
    format!("{JOB_START}{backend}")
}

pub fn parse_job_start(msg: &str) -> Option<&str> {
    msg.strip_prefix(JOB_START)
}

/// `🏁 job success in 1.2s`, or `❌ job <result> in 1.2s`.
pub fn job_end(result: &str, secs: f64) -> String {
    let icon = if result == "success" { "🏁" } else { "❌" };
    format!("{icon} job {result} in {secs:.1}s")
}

/// The result and seconds of a `job_end` line.
pub fn parse_job_end(msg: &str) -> Option<(&str, Option<f64>)> {
    let rest = msg
        .strip_prefix("🏁 job ")
        .or_else(|| msg.strip_prefix("❌ job "))?;
    let (result, secs) = rest.split_once(" in ")?;
    Some((result, secs.strip_suffix('s').and_then(|s| s.parse().ok())))
}

/// `⏭️  job skipped<why>`, where `why` starts with `: ` or ` (`.
pub fn job_skipped(why: &str) -> String {
    format!("{JOB_SKIPPED}{why}")
}

pub fn parse_job_skipped(msg: &str) -> Option<&str> {
    msg.strip_prefix(JOB_SKIPPED)
}

/// `⭐ <name>`: a step, or a pre or post hook, starts.
pub fn step_start(name: &str) -> String {
    format!("{STEP_START}{name}")
}

pub fn parse_step_start(msg: &str) -> Option<&str> {
    msg.strip_prefix(STEP_START)
}

/// `⏭️  <name>`: a step was skipped.
pub fn step_skipped(name: &str) -> String {
    format!("{STEP_SKIPPED}{name}")
}

pub fn parse_step_skipped(msg: &str) -> Option<&str> {
    msg.strip_prefix(STEP_SKIPPED)
}

/// How a step ended: ✅, 🛑 cancelled, ⚠️ failed but continuing (a failure
/// concluded as success by `continue-on-error`) or ❌, with its seconds if known.
pub fn step_end(name: &str, outcome: &str, conclusion: &str, secs: Option<f64>) -> String {
    let took = secs.map(|s| format!(" ({s:.1}s)")).unwrap_or_default();
    match (outcome, conclusion) {
        ("success", _) => format!("  ✅ {name}{took}"),
        ("cancelled", _) => format!("  🛑 {name} cancelled{took}"),
        (_, "success") => format!("  ⚠️  {name} failed, continuing{took}"),
        _ => format!("  ❌ {name}{took}"),
    }
}

/// The name, status (`success`, `failure`, `cancelled` or `warning`) and
/// seconds of a `step_end` line.
pub fn parse_step_end(msg: &str) -> Option<(&str, &'static str, Option<f64>)> {
    let (rest, status) = STEP_ENDS
        .iter()
        .find_map(|(p, st)| msg.strip_prefix(p).map(|rest| (rest, *st)))?;
    let (name, secs) = split_secs(rest);
    let name = match status {
        "warning" => name.strip_suffix(" failed, continuing").unwrap_or(name),
        "cancelled" => name.strip_suffix(" cancelled").unwrap_or(name),
        _ => name,
    };
    Some((name, status, secs))
}

/// `name (1.2s)` → (`name`, 1.2); `name` → (`name`, None).
fn split_secs(s: &str) -> (&str, Option<f64>) {
    if let Some(rest) = s.strip_suffix("s)") {
        if let Some((name, secs)) = rest.rsplit_once(" (") {
            if let Ok(v) = secs.parse::<f64>() {
                return (name, Some(v));
            }
        }
    }
    (s, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_ends_parse_back() {
        for (outcome, conclusion, status) in [
            ("success", "success", "success"),
            ("failure", "failure", "failure"),
            ("cancelled", "cancelled", "cancelled"),
            ("failure", "success", "warning"),
        ] {
            let line = step_end("Run make (x)", outcome, conclusion, Some(1.25));
            assert_eq!(
                parse_step_end(&line),
                Some(("Run make (x)", status, Some(1.2))),
                "{line}"
            );
            let line = step_end("Wait for a", outcome, conclusion, None);
            assert_eq!(parse_step_end(&line), Some(("Wait for a", status, None)));
        }
    }

    #[test]
    fn job_lines_parse_back() {
        assert_eq!(
            parse_job_end(&job_end("success", 2.04)),
            Some(("success", Some(2.0)))
        );
        assert_eq!(
            parse_job_end(&job_end("failure", 0.0)),
            Some(("failure", Some(0.0)))
        );
        assert_eq!(parse_job_start(&job_start("host")), Some("host"));
        assert_eq!(parse_job_skipped(&job_skipped(": why")), Some(": why"));
        assert_eq!(parse_step_start(&step_start("Run a")), Some("Run a"));
        assert_eq!(parse_step_skipped(&step_skipped("Run a")), Some("Run a"));
    }
}
