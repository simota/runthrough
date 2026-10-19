//! `--dashboard`: a local web UI over the run history in `history`, served by
//! the std-only HTTP server in `servers`.
use crate::history;
use crate::runner::owner_pid;
use crate::servers::{self, Request, Response};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

const PAGE: &str = include_str!("dashboard.html");

/// Serve until the process is stopped.
pub fn serve(cache: PathBuf, addr: &str, port: u16) -> Result<()> {
    let store = history::Store::new(&cache);
    let port = servers::serve(addr, port, Arc::new(move |req| handle(&store, &req)))?;
    eprintln!("📊 dashboard on http://{addr}:{port} (Ctrl-C to stop)");
    loop {
        std::thread::park();
    }
}

fn handle(store: &history::Store, req: &Request) -> Response {
    if req.method != "GET" {
        return Response::empty(405);
    }
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let found = match parts.as_slice() {
        [""] | ["index.html"] => {
            return Response {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: PAGE.as_bytes().to_vec(),
                encoding: None,
                file: None,
            }
        }
        ["api", "runs"] => Some(json!(store.list(100))),
        ["api", "resources"] => Some(resources()),
        ["api", "runs", id] if history::valid_id(id) => store.run(id),
        ["api", "runs", id, "job"] if history::valid_id(id) => {
            let q = |k: &str| req.query.get(k).map(String::as_str).unwrap_or("");
            // `skip=3,10`: the lines of each step the page already has.
            let skip: Vec<usize> = q("skip").split(',').map_while(|n| n.parse().ok()).collect();
            store.job(id, q("workflow"), q("job"), &skip)
        }
        _ => None,
    };
    match found {
        Some(v) => Response::json(200, v),
        None => Response::empty(404),
    }
}

/// What runthrough has on Docker: its containers, networks and volumes (named
/// `runthrough-…`), each with the process that made it when its name says so,
/// and whether that process still runs.
fn resources() -> Value {
    let list = |what: &[&str]| -> Result<Vec<Value>, String> {
        let out = Command::new("docker")
            .args(what)
            .args(["--filter", "name=runthrough-", "--format", "{{json .}}"])
            .output()
            .map_err(|e| format!("running docker: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    };
    let kinds = [
        ("containers", &["ps", "-a"][..]),
        ("networks", &["network", "ls"][..]),
        ("volumes", &["volume", "ls"][..]),
    ];
    let mut out = Map::new();
    for (kind, args) in kinds {
        match list(args) {
            Ok(items) => {
                out.insert(
                    kind.into(),
                    json!(items.iter().filter_map(resource).collect::<Vec<_>>()),
                );
            }
            Err(e) => {
                out.insert("error".into(), json!(e));
                break;
            }
        }
    }
    Value::Object(out)
}

/// One `docker … --format '{{json .}}'` row as the page shows it, or `None` for
/// a name the `name=` filter matched only in its middle.
fn resource(row: &Value) -> Option<Value> {
    let s = |k: &str| row[k].as_str().unwrap_or("");
    let name = [s("Names"), s("Name")]
        .into_iter()
        .find(|n| !n.is_empty())?;
    if !name.starts_with("runthrough-") {
        return None;
    }
    let ports: Vec<Value> = s("Ports")
        .split(", ")
        .filter_map(|p| {
            let (host, inner) = p.split_once("->")?;
            let (addr, port) = host.rsplit_once(':')?;
            Some(json!({"addr": addr, "port": port.parse::<u16>().ok()?, "to": inner}))
        })
        .collect();
    let pid = owner_pid(name);
    Some(json!({
        "name": name,
        "pid": pid,
        "alive": pid.is_some_and(|p| history::process_alive(p.into())),
        "image": s("Image"),
        "state": s("State"),
        "status": s("Status"),
        "created": s("CreatedAt"),
        "ports": ports,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn get(path: &str, query: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: HashMap::new(),
            body: vec![],
        }
    }

    #[test]
    fn routes_runs_and_rejects_other_paths() {
        let cache = tempfile::tempdir().unwrap();
        let r = history::Recorder::start(cache.path(), serde_json::Map::new()).unwrap();
        r.record(json!({"wf": "demo", "job": "build (1)", "msg": "🚀 start job on host"}));
        r.finish(true);
        let store = history::Store::new(cache.path());
        let page = handle(&store, &get("/", &[]));
        assert_eq!(page.status, 200);
        assert!(page.content_type.starts_with("text/html"));
        let runs: serde_json::Value =
            serde_json::from_slice(&handle(&store, &get("/api/runs", &[])).body).unwrap();
        let id = runs[0]["meta"]["id"].as_str().unwrap().to_string();
        assert_eq!(runs[0]["status"], "success");
        assert_eq!(
            handle(&store, &get(&format!("/api/runs/{id}"), &[])).status,
            200
        );
        let job = handle(
            &store,
            &get(
                &format!("/api/runs/{id}/job"),
                &[("workflow", "demo"), ("job", "build (1)")],
            ),
        );
        assert_eq!(job.status, 200);
        assert_eq!(handle(&store, &get("/api/runs/..%2Fx", &[])).status, 404);
        assert_eq!(
            handle(&store, &get("/api/runs/1/../../etc", &[])).status,
            404
        );
        let mut post = get("/api/runs", &[]);
        post.method = "POST".into();
        assert_eq!(handle(&store, &post).status, 405);
    }

    #[test]
    fn resources_carry_their_owner_and_published_ports() {
        assert_eq!(
            owner_pid("runthrough-test-ts-80467-0f20cbb8-9"),
            Some(80467)
        );
        assert_eq!(owner_pid("runthrough-step-97527-c74297d0-1"), Some(97527));
        assert_eq!(owner_pid("runthrough-p-1-deadbeef-0-db-net"), Some(1));
        assert_eq!(owner_pid("runthrough-toolcache-linux-ubuntu24"), None);
        assert_eq!(owner_pid("runthrough-1a2b3c4d"), None);

        let row = json!({
            "Names": "runthrough-services-42-0f20cbb8-3-db",
            "Image": "pgvector/pgvector:pg16",
            "State": "running",
            "Ports": "0.0.0.0:25432->5432/tcp, [::]:25432->5432/tcp, 6543/tcp",
        });
        let r = resource(&row).unwrap();
        assert_eq!(r["pid"], 42);
        // A run's resources are left behind only once its process is gone,
        // whether or not the run was recorded.
        let mine = format!("runthrough-step-{}-c74297d0-1", std::process::id());
        assert_eq!(resource(&json!({"Name": mine})).unwrap()["alive"], true);
        assert_eq!(
            r["ports"],
            json!([
                {"addr": "0.0.0.0", "port": 25432, "to": "5432/tcp"},
                {"addr": "[::]", "port": 25432, "to": "5432/tcp"},
            ])
        );
        // The `name=` filter matches anywhere in a name.
        assert!(resource(&json!({"Name": "my-runthrough-net"})).is_none());
        assert_eq!(
            resource(&json!({"Name": "runthrough-toolcache"})).unwrap()["pid"],
            Value::Null
        );
        assert_eq!(
            resource(&json!({"Name": "runthrough-toolcache"})).unwrap()["alive"],
            false
        );
    }
}
