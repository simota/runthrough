//! Job ordering (`needs:` DAG) and matrix expansion.

use crate::workflow::Workflow;
use anyhow::{bail, Result};
use indexmap::IndexSet;
use serde_json::{Map, Value};

/// `id` and every job it needs, directly or through other jobs.
pub fn with_needs(wf: &Workflow, id: &str) -> IndexSet<String> {
    let mut found = IndexSet::new();
    let mut todo = vec![id.to_string()];
    while let Some(j) = todo.pop() {
        if found.insert(j.clone()) {
            todo.extend(wf.jobs[&j].needs());
        }
    }
    found
}

/// Group jobs into stages; every job's `needs` are in earlier stages.
/// When `only` is given, that job and its transitive dependencies are planned.
pub fn stages(wf: &Workflow, only: Option<&str>) -> Result<Vec<Vec<String>>> {
    for (id, job) in &wf.jobs {
        for need in job.needs() {
            if !wf.jobs.contains_key(&need) {
                bail!("job '{id}' needs unknown job '{need}'");
            }
        }
    }
    let mut selected: IndexSet<String> = IndexSet::new();
    match only {
        Some(target) => {
            if !wf.jobs.contains_key(target) {
                bail!("job '{target}' not found in {}", wf.path.display());
            }
            selected = with_needs(wf, target);
        }
        None => selected.extend(wf.jobs.keys().cloned()),
    }

    let mut done: IndexSet<String> = IndexSet::new();
    let mut out = Vec::new();
    while done.len() < selected.len() {
        let stage: Vec<String> = wf
            .jobs
            .keys()
            .filter(|id| selected.contains(*id) && !done.contains(*id))
            .filter(|id| wf.jobs[*id].needs().iter().all(|n| done.contains(n)))
            .cloned()
            .collect();
        if stage.is_empty() {
            let rest: Vec<_> = selected
                .iter()
                .filter(|j| !done.contains(*j))
                .cloned()
                .collect();
            bail!("dependency cycle among jobs: {}", rest.join(", "));
        }
        done.extend(stage.iter().cloned());
        out.push(stage);
    }
    Ok(out)
}

/// Expand an evaluated `strategy.matrix` value into concrete combinations.
/// Returns a single empty combination when there is no matrix.
pub fn expand_matrix(matrix: &Value) -> Result<Vec<Map<String, Value>>> {
    let Value::Object(m) = matrix else {
        if matrix.is_null() {
            return Ok(vec![Map::new()]);
        }
        bail!("strategy.matrix must be a mapping, got {matrix}");
    };
    let mut combos: Vec<Map<String, Value>> = vec![Map::new()];
    let mut has_dims = false;
    for (key, vals) in m {
        if key == "include" || key == "exclude" {
            continue;
        }
        let Value::Array(vals) = vals else {
            bail!("matrix key '{key}' must be a list")
        };
        // As on GitHub (`fromJSON` of an empty list included): the job fails
        // instead of running once without a matrix.
        if vals.is_empty() {
            bail!("matrix vector '{key}' does not contain any values");
        }
        has_dims = true;
        let mut next = Vec::new();
        for c in &combos {
            for v in vals {
                let mut c = c.clone();
                c.insert(key.clone(), v.clone());
                next.push(c);
            }
        }
        combos = next;
    }
    if !has_dims {
        combos.clear();
    }

    if let Some(Value::Array(excludes)) = m.get("exclude") {
        for ex in excludes {
            if let Value::Object(e) = ex {
                if let Some(k) = e
                    .keys()
                    .find(|k| !matches!(m.get(*k), Some(Value::Array(_))))
                {
                    bail!("matrix exclude key '{k}' does not match any key within the matrix");
                }
            }
        }
        combos.retain(|c| {
            !excludes.iter().any(|ex| match ex {
                Value::Object(e) => e
                    .iter()
                    .all(|(k, v)| c.get(k).is_some_and(|cv| subset(v, cv))),
                _ => false,
            })
        });
    }

    let original: Vec<Map<String, Value>> = combos.clone();
    if let Some(Value::Array(includes)) = m.get("include") {
        for inc in includes {
            let Value::Object(inc) = inc else {
                bail!("matrix include entries must be mappings")
            };
            let mut matched = false;
            for (i, c) in combos.iter_mut().enumerate() {
                let orig = original.get(i);
                // may not overwrite an original matrix value
                let ok = inc.iter().all(|(k, v)| match orig.and_then(|o| o.get(k)) {
                    Some(ov) => ov == v,
                    None => true,
                });
                if ok && orig.is_some() {
                    matched = true;
                    for (k, v) in inc {
                        c.insert(k.clone(), v.clone());
                    }
                }
            }
            if !matched {
                combos.push(inc.clone());
            }
        }
    }
    if combos.is_empty() {
        combos.push(Map::new());
    }
    Ok(combos)
}

/// Partial match used by `exclude`: objects match when every key of `pat` matches.
fn subset(pat: &Value, v: &Value) -> bool {
    match (pat, v) {
        (Value::Object(p), Value::Object(o)) => p
            .iter()
            .all(|(k, pv)| o.get(k).is_some_and(|ov| subset(pv, ov))),
        _ => pat == v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exclude_with_unknown_key_is_an_error() {
        let m = serde_json::json!({"os": ["a"], "exclude": [{"node": 1}]});
        let err = expand_matrix(&m).unwrap_err().to_string();
        assert!(err.contains("'node'"), "{err}");
    }

    #[test]
    fn cartesian_exclude_include() {
        let m = json!({
            "os": ["linux", "mac"],
            "node": [16, 18],
            "exclude": [{"os": "mac", "node": 16}],
            "include": [
                {"os": "linux", "experimental": true},
                {"os": "windows", "node": 20}
            ]
        });
        let c = expand_matrix(&m).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(
            Value::Object(c[0].clone()),
            json!({"os": "linux", "node": 16, "experimental": true})
        );
        assert_eq!(
            Value::Object(c[1].clone()),
            json!({"os": "linux", "node": 18, "experimental": true})
        );
        assert_eq!(
            Value::Object(c[2].clone()),
            json!({"os": "mac", "node": 18})
        );
        assert_eq!(
            Value::Object(c[3].clone()),
            json!({"os": "windows", "node": 20})
        );
    }

    #[test]
    fn include_only() {
        let c = expand_matrix(&json!({"include": [{"a": 1}, {"a": 2}]})).unwrap();
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn no_matrix() {
        assert_eq!(expand_matrix(&Value::Null).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_vector_is_an_error() {
        let e = expand_matrix(&json!({"os": [], "include": [{"os": "x"}]})).unwrap_err();
        assert_eq!(
            e.to_string(),
            "matrix vector 'os' does not contain any values"
        );
    }
}
