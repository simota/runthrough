//! Local stand-ins for the GitHub services actions talk to:
//!
//! - a cache server speaking the `@actions/cache` v1 API (`ACTIONS_CACHE_URL`), and
//! - an artifact server speaking the `@actions/artifact` v2 API used by
//!   `actions/upload-artifact@v4` / `download-artifact@v4` (`ACTIONS_RESULTS_URL`):
//!   Twirp JSON calls plus Azure-blob-style block uploads to a signed URL, and
//!   the v1 API of the v3 actions (`ACTIONS_RUNTIME_URL`): a file container
//!   per run with chunked, optionally gzipped, uploads.
//!
//! Both are plain HTTP/1.1 servers on std threads; jobs get the URLs and a runtime
//! token through environment variables.

use crate::actions::base64;
use crate::backend::Engine;
use crate::workflow::CacheMode;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

// ------------------------------------------------------------------ HTTP

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// `Content-Encoding`, for gzipped v3 artifact files.
    pub encoding: Option<&'static str>,
    /// Sent after the headers instead of `body`, without reading it into memory.
    pub file: Option<std::fs::File>,
}

impl Response {
    pub(crate) fn json(status: u16, v: Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: v.to_string().into_bytes(),
            encoding: None,
            file: None,
        }
    }

    pub(crate) fn empty(status: u16) -> Response {
        Response {
            status,
            content_type: "text/plain",
            body: vec![],
            encoding: None,
            file: None,
        }
    }

    /// The file at `path`, streamed; 404 when it cannot be opened.
    fn file(path: &Path, content_type: &'static str, encoding: Option<&'static str>) -> Response {
        match std::fs::File::open(path) {
            Ok(f) => Response {
                status: 200,
                content_type,
                body: vec![],
                encoding,
                file: Some(f),
            },
            Err(_) => Response::empty(404),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Decoded on bytes: a `%` before a multibyte character is kept as is.
        if let (b'%', Some(hi), Some(lo)) = (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("bad request line");
    };
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)?;
            if n == 0 {
                let mut end = String::new();
                reader.read_line(&mut end)?;
                break;
            }
            read_body(reader, &mut body, n)?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
        }
    } else if let Some(n) = headers.get("content-length") {
        read_body(reader, &mut body, n.parse()?)?;
    }
    Ok(Some(Request {
        method: method.to_string(),
        path: percent_decode(path),
        query,
        headers,
        body,
    }))
}

/// Append `n` bytes of body to `body`, growing it only as bytes arrive: a
/// length the client announces but never sends must not be allocated up front
/// (a failed allocation aborts the whole process, not just this thread).
fn read_body(reader: &mut impl BufRead, body: &mut Vec<u8>, n: usize) -> Result<()> {
    let start = body.len();
    // Uploads come in chunks of a few MB: reserve that much at once, and grow
    // past it only for bytes that really arrive.
    body.reserve(n.min(64 << 20));
    reader.take(n as u64).read_to_end(body)?;
    if body.len() - start != n {
        bail!(
            "request body ended after {} of {n} bytes",
            body.len() - start
        );
    }
    Ok(())
}

fn write_response(stream: &mut impl Write, r: &Response) -> std::io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let body: &[u8] = if r.status == 204 { &[] } else { &r.body };
    let len = match &r.file {
        Some(f) => f.metadata()?.len(),
        None => body.len() as u64,
    };
    let encoding = r
        .encoding
        .map(|e| format!("Content-Encoding: {e}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{encoding}Cache-Control: no-store\r\nETag: \"0x1\"\r\nLast-Modified: Thu, 01 Jan 2026 00:00:00 GMT\r\nx-ms-request-id: runthrough\r\nx-ms-version: 2024-11-04\r\n\r\n",
        r.status,
        r.content_type,
        len
    )?;
    match &r.file {
        Some(f) => {
            std::io::copy(&mut &*f, stream)?;
        }
        None => stream.write_all(body)?,
    }
    stream.flush()
}

pub(crate) type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Serve `handler` on `addr:port` in background threads; returns the bound port.
pub(crate) fn serve(addr: &str, port: u16, handler: Handler) -> Result<u16> {
    let listener =
        TcpListener::bind((addr, port)).with_context(|| format!("binding {addr}:{port}"))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let Ok(mut writer) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                while let Ok(Some(req)) = read_request(&mut reader) {
                    let close = req
                        .headers
                        .get("connection")
                        .is_some_and(|c| c.eq_ignore_ascii_case("close"));
                    let resp = handler(req);
                    if write_response(&mut writer, &resp).is_err() || close {
                        break;
                    }
                }
            });
        }
    });
    Ok(port)
}

// ------------------------------------------------------------------ cache (v1 API)

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    id: u64,
    /// Stored lowercased: keys are case-insensitive, as in act.
    key: String,
    version: String,
    complete: bool,
    created: u64,
    /// Last restore or upload, in ms; drives garbage collection.
    #[serde(default)]
    used: u64,
}

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 24 * 60 * MINUTE_MS;

struct CacheStore {
    dir: PathBuf,
    entries: Vec<CacheEntry>,
    next_id: u64,
}

impl CacheStore {
    fn open(dir: &Path) -> Result<CacheStore> {
        std::fs::create_dir_all(dir.join("blobs"))
            .with_context(|| format!("creating cache directory {}", dir.display()))?;
        let entries: Vec<CacheEntry> = std::fs::read(dir.join("index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        let mut store = CacheStore {
            dir: dir.to_path_buf(),
            entries,
            next_id,
        };
        store.gc(now());
        Ok(store)
    }

    /// act's rules: drop uploads abandoned for 5 minutes, entries unused for 7
    /// days or created over 30 days ago, and older entries of the same key and
    /// version (unless used in the last 5 minutes).
    fn gc(&mut self, now: u64) {
        let last_used = |e: &CacheEntry| e.used.max(e.created);
        let newest: std::collections::HashMap<(String, String), u64> = self
            .entries
            .iter()
            .filter(|e| e.complete)
            .fold(std::collections::HashMap::new(), |mut m, e| {
                let slot = m.entry((e.key.clone(), e.version.clone())).or_insert(e.id);
                if e.id > *slot {
                    *slot = e.id;
                }
                m
            });
        let (keep, drop): (Vec<CacheEntry>, Vec<CacheEntry>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|e| {
                let recent = now.saturating_sub(last_used(e)) < 5 * MINUTE_MS;
                let abandoned = !e.complete && !recent;
                let stale = now.saturating_sub(last_used(e)) > 7 * DAY_MS
                    || now.saturating_sub(e.created) > 30 * DAY_MS;
                let superseded = e.complete
                    && newest.get(&(e.key.clone(), e.version.clone())) != Some(&e.id)
                    && !recent;
                !(abandoned || stale || superseded)
            });
        for e in &drop {
            let _ = std::fs::remove_file(self.blob(e.id));
        }
        self.entries = keep;
        if !drop.is_empty() {
            self.save();
        }
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(&self.entries) {
            let _ = std::fs::write(self.dir.join("index.json"), b);
        }
    }

    fn blob(&self, id: u64) -> PathBuf {
        self.dir.join("blobs").join(id.to_string())
    }

    /// Exact key first, then the newest entry whose key starts with a restore key.
    fn lookup(&self, keys: &[String], version: &str) -> Option<&CacheEntry> {
        let usable = |e: &&CacheEntry| e.complete && e.version == version;
        for k in keys.iter().map(|k| k.to_lowercase()) {
            let k = &k;
            if let Some(e) = self
                .entries
                .iter()
                .filter(usable)
                .filter(|e| &e.key == k)
                .max_by_key(|e| e.created)
            {
                return Some(e);
            }
            if let Some(e) = self
                .entries
                .iter()
                .filter(usable)
                .filter(|e| e.key.starts_with(k.as_str()))
                .max_by_key(|e| e.created)
            {
                return Some(e);
            }
        }
        None
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Random hex used as a per-run path prefix, so only the jobs of this run can
/// use the cache server.
pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if read.is_err() {
        let seed = format!("{}-{}", std::process::id(), now());
        use sha2::Digest;
        bytes.copy_from_slice(&sha2::Sha256::digest(seed.as_bytes())[..16]);
    }
    hex::encode(bytes)
}

/// A job's `cache-mode`, carried in its `ACTIONS_CACHE_URL` as `/<token>/<mode>/`
/// (no segment for `write`).
fn cache_mode_segment(mode: CacheMode) -> String {
    match mode {
        CacheMode::Write => String::new(),
        m => format!("{}/", m.name()),
    }
}

fn cache_handler(store: Arc<Mutex<CacheStore>>, token: &str, req: Request) -> Response {
    let Some(after_token) = req.path.strip_prefix(&format!("/{token}/")) else {
        return Response::empty(404);
    };
    let (mode, after_mode) = [CacheMode::Read, CacheMode::WriteOnly, CacheMode::Disabled]
        .into_iter()
        .find_map(|m| Some((m, after_token.strip_prefix(&cache_mode_segment(m))?)))
        .unwrap_or((CacheMode::Write, after_token));
    let Some(rest) = after_mode.strip_prefix("_apis/artifactcache/") else {
        return Response::empty(404);
    };
    let (restore, save) = (mode.can_restore(), mode.can_save());
    let mut store = store.lock().unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    match (
        req.method.as_str(),
        rest.split('/').collect::<Vec<_>>().as_slice(),
    ) {
        // A denied restore is a cache miss; a denied save is refused, which the
        // toolkit logs as a `ReserveCacheError` ("Failed to save: ...") without
        // failing the step.
        ("GET", ["cache"]) if !restore => Response::empty(204),
        ("POST" | "PATCH", ["caches", ..]) if !save => Response::json(
            403,
            json!({"message": format!("cache-mode {} does not allow saving", mode.name())}),
        ),
        ("GET", ["artifacts", _]) if !restore => Response::json(
            403,
            json!({"message": format!("cache-mode {} does not allow restoring", mode.name())}),
        ),
        ("GET", ["cache"]) => {
            let keys: Vec<String> = req
                .query
                .get("keys")
                .map(|k| k.split(',').map(String::from).collect())
                .unwrap_or_default();
            let version = req.query.get("version").cloned().unwrap_or_default();
            let found = store.lookup(&keys, &version).map(|e| (e.id, e.key.clone()));
            match found {
                Some((id, key)) => {
                    if let Some(e) = store.entries.iter_mut().find(|e| e.id == id) {
                        e.used = now();
                    }
                    store.save();
                    let host = req.headers.get("host").cloned().unwrap_or_default();
                    Response::json(
                        200,
                        json!({
                            "cacheKey": key, "scope": "refs/heads/main",
                            "archiveLocation": format!(
                                "http://{host}/{token}/{}_apis/artifactcache/artifacts/{id}",
                                cache_mode_segment(mode)
                            ),
                        }),
                    )
                }
                None => Response::empty(204),
            }
        }
        ("POST", ["caches"]) => {
            let key = body["key"].as_str().unwrap_or_default().to_lowercase();
            let version = body["version"].as_str().unwrap_or_default().to_string();
            if store
                .entries
                .iter()
                .any(|e| e.complete && e.key == key && e.version == version)
            {
                return Response::json(409, json!({"message": "cache already exists"}));
            }
            let id = store.next_id;
            store.next_id += 1;
            store
                .entries
                .retain(|e| !(e.key == key && e.version == version));
            store.entries.push(CacheEntry {
                id,
                key,
                version,
                complete: false,
                created: now(),
                used: now(),
            });
            let _ = std::fs::write(store.blob(id), b"");
            Response::json(201, json!({"cacheId": id}))
        }
        ("PATCH", ["caches", id]) => {
            let Ok(id) = id.parse::<u64>() else {
                return Response::empty(400);
            };
            // Only a pending reservation takes data, not a committed entry.
            if !store.entries.iter().any(|e| e.id == id && !e.complete) {
                return Response::empty(404);
            }
            // Other jobs' cache calls go on while this chunk is written.
            let blob = store.blob(id);
            drop(store);
            // Content-Range: bytes start-end/*
            let start: u64 = req
                .headers
                .get("content-range")
                .and_then(|r| r.strip_prefix("bytes "))
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let written = std::fs::OpenOptions::new()
                .write(true)
                .open(blob)
                .and_then(|mut f| {
                    f.seek(SeekFrom::Start(start))?;
                    f.write_all(&req.body)
                });
            match written {
                Ok(()) => Response::empty(204),
                Err(_) => Response::empty(404),
            }
        }
        ("POST", ["caches", id]) => {
            let Ok(id) = id.parse::<u64>() else {
                return Response::empty(400);
            };
            match store.entries.iter_mut().find(|e| e.id == id && !e.complete) {
                Some(e) => {
                    e.complete = true;
                    e.used = now();
                    store.save();
                    store.gc(now());
                    Response::empty(204)
                }
                None => Response::empty(404),
            }
        }
        ("GET", ["artifacts", id]) => match id.parse::<u64>().map(|id| store.blob(id)) {
            Ok(p) => Response::file(&p, "application/octet-stream", None),
            Err(_) => Response::empty(400),
        },
        _ => Response::empty(404),
    }
}

// ------------------------------------------------------------------ artifacts (v4 API)

struct Artifact {
    id: u64,
    run: String,
    name: String,
    size: u64,
    finalized: bool,
    created: String,
}

struct ArtifactStore {
    dir: PathBuf,
    artifacts: Vec<Artifact>,
    /// Per-run HMAC key for the v4 upload/download URLs.
    key: Vec<u8>,
}

/// Signed URLs stay valid this long, as in act.
const SIGNED_URL_TTL_MS: u64 = 60 * MINUTE_MS;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

impl ArtifactStore {
    /// `?expires=…&sig=…` for `/{kind}/{id}`.
    fn sign(&self, kind: &str, id: u64) -> String {
        let expires = now() + SIGNED_URL_TTL_MS;
        let sig = hex::encode(hmac_sha256(
            &self.key,
            format!("{kind}/{id}/{expires}").as_bytes(),
        ));
        format!("expires={expires}&sig={sig}")
    }

    fn verify(&self, kind: &str, id: u64, query: &HashMap<String, String>) -> bool {
        let (Some(expires), Some(sig)) = (query.get("expires"), query.get("sig")) else {
            return false;
        };
        let fresh = expires.parse::<u64>().is_ok_and(|e| e >= now());
        let want = hex::encode(hmac_sha256(
            &self.key,
            format!("{kind}/{id}/{expires}").as_bytes(),
        ));
        fresh && want == *sig
    }
}

impl ArtifactStore {
    fn file(&self, a: &Artifact) -> PathBuf {
        self.dir.join(&a.run).join(&a.name)
    }

    fn blocks_dir(&self, id: u64) -> PathBuf {
        self.dir.join(".blocks").join(id.to_string())
    }
}

fn rfc3339_now() -> String {
    // Seconds since the epoch rendered as a UTC timestamp.
    let secs = (now() / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Suffix marking a v3 artifact file stored gzipped (as act does).
const GZIP_SUFFIX: &str = ".gz__";

/// A relative path with no `..`, absolute or empty components.
fn safe_rel(p: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = p
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    (!parts.is_empty() && parts.iter().all(|c| *c != ".."))
        .then(|| parts.iter().collect::<PathBuf>())
}

/// The v1 artifact API used by `upload-artifact@v3` / `download-artifact@v3`.
fn artifact_v3(dir: &Path, host: &str, req: &Request) -> Response {
    let root = dir.join("v3");
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    let run_dir = |run: &str| safe_rel(run).map(|r| root.join(r));
    match (req.method.as_str(), parts.as_slice()) {
        // _apis/pipelines/workflows/{run}/artifacts
        ("POST", ["_apis", "pipelines", "workflows", run, "artifacts"]) => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let name = body["Name"].as_str().unwrap_or_default();
            Response::json(
                200,
                json!({
                    "containerId": run, "size": -1, "type": "actions_storage", "name": name,
                    "fileContainerResourceUrl": format!("http://{host}/v3/upload/{run}"),
                    "url": format!("http://{host}/_apis/pipelines/workflows/{run}/artifacts"),
                }),
            )
        }
        ("PATCH", ["_apis", "pipelines", "workflows", _, "artifacts"]) => {
            Response::json(200, json!({}))
        }
        ("GET", ["_apis", "pipelines", "workflows", run, "artifacts"]) => {
            let Some(d) = run_dir(run) else {
                return Response::empty(400);
            };
            let mut names: Vec<String> = std::fs::read_dir(&d)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let value: Vec<Value> = names
                .iter()
                .map(|n| json!({"name": n, "fileContainerResourceUrl": format!("http://{host}/v3/download/{run}")}))
                .collect();
            Response::json(200, json!({"count": value.len(), "value": value}))
        }
        ("PUT", ["v3", "upload", run]) => {
            let (Some(d), Some(item)) = (
                run_dir(run),
                req.query.get("itemPath").and_then(|i| safe_rel(i)),
            ) else {
                return Response::empty(400);
            };
            let gzip = req
                .headers
                .get("content-encoding")
                .is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
            let mut file = d.join(item);
            if gzip {
                file.as_mut_os_string().push(GZIP_SUFFIX);
            }
            // Content-Range: bytes start-end/total
            let start: u64 = req
                .headers
                .get("content-range")
                .and_then(|r| r.strip_prefix("bytes "))
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let written = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(file.parent().unwrap_or(&d))?;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(start == 0)
                    .open(&file)?;
                f.seek(SeekFrom::Start(start))?;
                f.write_all(&req.body)
            })();
            match written {
                Ok(()) => Response::json(200, json!({})),
                Err(_) => Response::empty(400),
            }
        }
        ("GET", ["v3", "download", run]) => {
            let (Some(d), Some(name)) = (
                run_dir(run),
                req.query.get("itemPath").and_then(|i| safe_rel(i)),
            ) else {
                return Response::empty(400);
            };
            let base = d.join(&name);
            let value: Vec<Value> = walkdir::WalkDir::new(&base)
                .sort_by_file_name()
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .filter_map(|e| {
                    let rel = e
                        .path()
                        .strip_prefix(&d)
                        .ok()?
                        .to_string_lossy()
                        .replace('\\', "/");
                    let path = rel.strip_suffix(GZIP_SUFFIX).unwrap_or(&rel).to_string();
                    Some(json!({
                        "path": path, "itemType": "file", "fileLength": e.metadata().ok()?.len(),
                        "contentLocation": format!("http://{host}/v3/artifact/{run}/{rel}"),
                    }))
                })
                .collect();
            Response::json(200, json!({"count": value.len(), "value": value}))
        }
        ("GET", ["v3", "artifact", run, rest @ ..]) => {
            let (Some(d), Some(rel)) = (run_dir(run), safe_rel(&rest.join("/"))) else {
                return Response::empty(400);
            };
            let gzip = rel.to_string_lossy().ends_with(GZIP_SUFFIX);
            Response::file(
                &d.join(&rel),
                "application/octet-stream",
                gzip.then_some("gzip"),
            )
        }
        _ => Response::empty(404),
    }
}

fn artifact_handler(shared: Arc<Mutex<ArtifactStore>>, req: Request) -> Response {
    let host = req.headers.get("host").cloned().unwrap_or_default();
    let mut store = shared.lock().unwrap();
    if req.path.starts_with("/_apis/pipelines/") || req.path.starts_with("/v3/") {
        // The v3 API keeps no state beyond its files.
        let dir = store.dir.clone();
        drop(store);
        return artifact_v3(&dir, &host, &req);
    }
    match req
        .path
        .strip_prefix("/twirp/github.actions.results.api.v1.ArtifactService/")
    {
        Some(method) => twirp(&mut store, method, &req, &host),
        None => blob(&shared, store, &req),
    }
}

/// The v4 artifact API's Twirp calls (`@actions/artifact` v2).
fn twirp(store: &mut ArtifactStore, method: &str, req: &Request, host: &str) -> Response {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    // The toolkit sends proto field names (`workflow_run_backend_id`);
    // protobuf JSON also allows the lowerCamelCase form.
    let field = |camel: &str, snake: &str| -> Value {
        body.get(snake)
            .or_else(|| body.get(camel))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let run = field("workflowRunBackendId", "workflow_run_backend_id")
        .as_str()
        .unwrap_or_default()
        .to_string();
    let name = body["name"].as_str().unwrap_or_default().to_string();
    let find = |s: &ArtifactStore| {
        s.artifacts
            .iter()
            .position(|a| a.run == run && a.name == name && a.finalized)
    };
    match method {
        "CreateArtifact" => {
            if name.is_empty() || name.contains(['/', '\\']) || name == ".." {
                return twirp_error(400, "invalid_argument", "invalid artifact name");
            }
            // The run ID becomes a directory under the artifact root: one
            // plain component, so `..`, an absolute path or a Windows drive
            // (`C:`) cannot escape it.
            if !plain_component(&run) {
                return twirp_error(400, "invalid_argument", "invalid workflow run backend ID");
            }
            if find(store).is_some() {
                return twirp_error(
                    409,
                    "already_exists",
                    &format!(
                        "an artifact with this name already exists on the workflow run: {name}"
                    ),
                );
            }
            let id = store.artifacts.iter().map(|a| a.id).max().unwrap_or(0) + 1;
            store
                .artifacts
                .retain(|a| !(a.run == run && a.name == name));
            store.artifacts.push(Artifact {
                id,
                run,
                name,
                size: 0,
                finalized: false,
                created: rfc3339_now(),
            });
            let _ = std::fs::remove_dir_all(store.blocks_dir(id));
            Response::json(
                200,
                json!({"ok": true, "signedUploadUrl": format!("http://{host}/upload/{id}?{}", store.sign("upload", id))}),
            )
        }
        "FinalizeArtifact" => {
            let size = json_u64(&body["size"]).unwrap_or(0);
            match store
                .artifacts
                .iter_mut()
                .find(|a| a.run == run && a.name == name && !a.finalized)
            {
                Some(a) => {
                    a.finalized = true;
                    if size > 0 {
                        a.size = size;
                    }
                    Response::json(200, json!({"ok": true, "artifactId": a.id.to_string()}))
                }
                None => twirp_error(404, "not_found", "artifact not found"),
            }
        }
        "ListArtifacts" => {
            let name_filter = field("nameFilter", "name_filter");
            let name_filter = name_filter.as_str();
            let id_filter = json_u64(&field("idFilter", "id_filter"));
            let list: Vec<Value> = store
                .artifacts
                .iter()
                .filter(|a| a.finalized && a.run == run)
                .filter(|a| name_filter.is_none_or(|n| a.name == n))
                .filter(|a| id_filter.is_none_or(|i| a.id == i))
                .map(|a| {
                    json!({
                        "workflowRunBackendId": a.run, "workflowJobRunBackendId": "job",
                        "databaseId": a.id.to_string(), "name": a.name,
                        "size": a.size.to_string(), "createdAt": a.created,
                    })
                })
                .collect();
            Response::json(200, json!({"artifacts": list}))
        }
        "GetSignedArtifactURL" => match find(store) {
            Some(i) => Response::json(
                200,
                json!({"signedUrl": format!("http://{host}/download/{}?{}", store.artifacts[i].id, store.sign("download", store.artifacts[i].id))}),
            ),
            None => twirp_error(404, "not_found", "artifact not found"),
        },
        "DeleteArtifact" => match find(store) {
            Some(i) => {
                let a = store.artifacts.remove(i);
                let _ = std::fs::remove_file(store.file(&a));
                Response::json(200, json!({"ok": true, "artifactId": a.id.to_string()}))
            }
            None => twirp_error(404, "not_found", "artifact not found"),
        },
        _ => twirp_error(404, "bad_route", method),
    }
}

/// Azure-blob-style block uploads and downloads at the URLs `twirp` signs.
fn blob(
    shared: &Arc<Mutex<ArtifactStore>>,
    store: MutexGuard<'_, ArtifactStore>,
    req: &Request,
) -> Response {
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    match (req.method.as_str(), parts.as_slice()) {
        ("PUT", ["upload", id]) => {
            let Ok(id) = id.parse::<u64>() else {
                return Response::empty(400);
            };
            if !store.verify("upload", id, &req.query) {
                return Response::empty(401);
            }
            let Some(idx) = store.artifacts.iter().position(|a| a.id == id) else {
                return Response::empty(404);
            };
            let blocks = store.blocks_dir(id);
            let file = store.file(&store.artifacts[idx]);
            let dir = store.dir.clone();
            // Blocks are written and joined while other jobs' calls go on; only
            // the size is set under the lock.
            drop(store);
            let comp = req.query.get("comp").map(String::as_str);
            let result: Result<Option<u64>> = (|| {
                Ok(match comp {
                    Some("block") => {
                        let block = req.query.get("blockid").cloned().unwrap_or_default();
                        std::fs::create_dir_all(&blocks)?;
                        std::fs::write(blocks.join(hex::encode(block)), &req.body)?;
                        None
                    }
                    Some("blocklist") => {
                        let xml = String::from_utf8_lossy(&req.body);
                        std::fs::create_dir_all(file.parent().unwrap_or(&dir))?;
                        let mut out = std::fs::File::create(&file)?;
                        let mut size = 0u64;
                        for chunk in xml.split('>').skip(1) {
                            let Some((block, _)) = chunk.split_once('<') else {
                                continue;
                            };
                            let block = block.trim();
                            if block.is_empty() {
                                continue;
                            }
                            let mut data = std::fs::File::open(blocks.join(hex::encode(block)))?;
                            size += std::io::copy(&mut data, &mut out)?;
                        }
                        let _ = std::fs::remove_dir_all(&blocks);
                        Some(size)
                    }
                    _ => {
                        std::fs::create_dir_all(file.parent().unwrap_or(&dir))?;
                        std::fs::write(&file, &req.body)?;
                        Some(req.body.len() as u64)
                    }
                })
            })();
            match result {
                Ok(size) => {
                    if let Some(size) = size {
                        let mut store = shared.lock().unwrap();
                        if let Some(a) = store.artifacts.iter_mut().find(|a| a.id == id) {
                            a.size = size;
                        }
                    }
                    Response::empty(201)
                }
                Err(_) => Response::empty(400),
            }
        }
        ("GET", ["download", id]) => {
            let Ok(id) = id.parse::<u64>() else {
                return Response::empty(400);
            };
            if !store.verify("download", id, &req.query) {
                return Response::empty(401);
            }
            let Some(a) = store.artifacts.iter().find(|a| a.id == id) else {
                return Response::empty(404);
            };
            Response::file(&store.file(a), "application/zip", None)
        }
        _ => Response::empty(404),
    }
}

/// A Twirp error response.
fn twirp_error(status: u16, code: &str, msg: &str) -> Response {
    Response::json(status, json!({"code": code, "msg": msg}))
}

/// A protobuf JSON `uint64`: a string, or a number.
fn json_u64(v: &Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64())
}

/// One plain path component: no separator, `..`, `.` or Windows drive (`C:`),
/// so it cannot leave the directory it is joined to.
fn plain_component(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '\\', ':']) && s != ".." && s != "."
}

// ------------------------------------------------------------------ wiring

/// Running servers and how jobs reach them.
#[derive(Clone)]
pub struct Servers {
    cache_port: Option<u16>,
    /// Per-run path prefix of the cache API.
    cache_token: String,
    cache_external: Option<String>,
    artifact_port: Option<u16>,
    run_id: String,
}

fn base64url(data: &[u8]) -> String {
    base64(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

impl Servers {
    pub fn start(
        cli: &crate::Cli,
        base: &Path,
        engine: Engine,
        force_host: bool,
    ) -> Result<Servers> {
        // Apple containers reach the host through the vmnet gateway, so the
        // servers must listen beyond loopback there.
        let default_addr = if engine.reaches_host_through_gateway() && !force_host {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };
        let mut servers = Servers {
            cache_port: None,
            cache_external: None,
            artifact_port: None,
            run_id: format!("{}-{}", std::process::id(), now()),
            cache_token: random_token(),
        };
        if !cli.no_cache_server {
            let dir = cli
                .cache_server_path
                .clone()
                .unwrap_or_else(|| base.join("cache"));
            let started = CacheStore::open(&dir).and_then(|store| {
                let store = Arc::new(Mutex::new(store));
                let token = servers.cache_token.clone();
                serve(
                    cli.cache_server_addr.as_deref().unwrap_or(default_addr),
                    cli.cache_server_port,
                    Arc::new(move |req| cache_handler(store.clone(), &token, req)),
                )
            });
            match started {
                Ok(p) => {
                    servers.cache_port = Some(p);
                    servers.cache_external = cli
                        .cache_server_external_url
                        .as_deref()
                        .map(|u| u.trim_end_matches('/').to_string());
                }
                // Jobs still run; only actions/cache loses its backend.
                Err(e) => eprintln!("⚠️  cache server disabled: {e:#}"),
            }
        }
        if let Some(path) = &cli.artifact_server_path {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating artifact directory {}", path.display()))?;
            let store = Arc::new(Mutex::new(ArtifactStore {
                dir: path.canonicalize()?,
                artifacts: vec![],
                key: hex::decode(random_token()).unwrap_or_default(),
            }));
            let p = serve(
                cli.artifact_server_addr.as_deref().unwrap_or(default_addr),
                cli.artifact_server_port,
                Arc::new(move |req| artifact_handler(store.clone(), req)),
            )?;
            servers.artifact_port = Some(p);
        }
        Ok(servers)
    }

    /// The ports this run listens on, recorded for `--dashboard`'s resources page.
    pub fn ports(&self) -> Value {
        json!({"cache": self.cache_port, "artifacts": self.artifact_port})
    }

    /// Environment variables for a job; `host` is how the job reaches this machine.
    /// `cache_mode` is the job's `cache-mode` (`read`, `write`, `write-only`, `none`).
    pub fn job_env(&self, host: &str, job: &str, cache_mode: CacheMode) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if self.cache_port.is_none() && self.artifact_port.is_none() {
            return env;
        }
        let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        // BuildKit's `type=gha` cache refuses a token without `ac`, `exp` or `nbf`.
        // `ac` (a JSON string, as GitHub sends it) grants read and write on the
        // scope the cache server reports; BuildKit writes only to a writable one.
        let claims = json!({
            "scp": format!("Actions.ExampleScope Actions.Results:{}:{job}", self.run_id),
            "ac": json!([{"Scope": "refs/heads/main", "Permission": 3}]).to_string(),
            "nbf": 0,
            "exp": 4_102_444_800u64,
        });
        let token = format!(
            "{header}.{}.runthrough",
            base64url(claims.to_string().as_bytes())
        );
        // As in act, values from the caller's environment win.
        let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        env.push((
            "ACTIONS_RUNTIME_TOKEN".into(),
            from_env("ACTIONS_RUNTIME_TOKEN").unwrap_or(token),
        ));
        if let Some(p) = self.cache_port {
            let path = format!("{}/{}", self.cache_token, cache_mode_segment(cache_mode));
            let url = match &self.cache_external {
                Some(u) => format!("{u}/{path}"),
                None => format!("http://{host}:{p}/{path}"),
            };
            env.push(("ACTIONS_CACHE_URL".into(), url));
        }
        if let Some(p) = self.artifact_port {
            env.push(("ACTIONS_RESULTS_URL".into(), format!("http://{host}:{p}/")));
            env.push((
                "ACTIONS_RUNTIME_URL".into(),
                from_env("ACTIONS_RUNTIME_URL").unwrap_or_else(|| format!("http://{host}:{p}/")),
            ));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_mode_limits_restore_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(CacheStore::open(dir.path()).unwrap()));
        let req = |method: &str, path: &str, query: &[(&str, &str)], body: Value| Request {
            method: method.into(),
            path: format!(
                "/tok/{path}_apis/artifactcache/{}",
                if method == "GET" { "cache" } else { "caches" }
            ),
            query: query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: HashMap::new(),
            body: body.to_string().into_bytes(),
        };
        let reserve = |mode: &str, key: &str| {
            cache_handler(
                store.clone(),
                "tok",
                req("POST", mode, &[], json!({"key": key, "version": "v"})),
            )
            .status
        };
        assert_eq!(reserve("read/", "a"), 403);
        assert_eq!(reserve("none/", "a"), 403);
        assert_eq!(reserve("write-only/", "a"), 201);
        assert_eq!(reserve("", "b"), 201);
        for e in &mut store.lock().unwrap().entries {
            e.complete = true;
        }
        let lookup = |mode: &str| {
            cache_handler(
                store.clone(),
                "tok",
                req("GET", mode, &[("keys", "a"), ("version", "v")], Value::Null),
            )
            .status
        };
        assert_eq!(lookup(""), 200);
        assert_eq!(lookup("read/"), 200);
        assert_eq!(lookup("write-only/"), 204);
        assert_eq!(lookup("none/"), 204);
    }

    /// The upload, finalize and download endpoints honor `cache-mode` too, and
    /// only a pending reservation takes data: a mode cannot be bypassed by
    /// addressing an entry by its (sequential) ID.
    #[test]
    fn cache_mode_guards_every_data_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(CacheStore::open(dir.path()).unwrap()));
        let call = |method: &str, path: &str, body: &[u8]| {
            let mut headers = HashMap::new();
            headers.insert("content-range".to_string(), "bytes 0-3/*".to_string());
            cache_handler(
                store.clone(),
                "tok",
                Request {
                    method: method.into(),
                    path: format!("/tok/{path}"),
                    query: HashMap::new(),
                    headers,
                    body: body.to_vec(),
                },
            )
            .status
        };
        let reserve = json!({"key": "k", "version": "v"}).to_string();
        assert_eq!(
            call("POST", "_apis/artifactcache/caches", reserve.as_bytes()),
            201
        );
        let id = store.lock().unwrap().entries[0].id;
        let entry = format!("_apis/artifactcache/caches/{id}");
        let download = format!("_apis/artifactcache/artifacts/{id}");
        for mode in ["read/", "none/"] {
            assert_eq!(call("PATCH", &format!("{mode}{entry}"), b"evil"), 403);
            assert_eq!(call("POST", &format!("{mode}{entry}"), b""), 403);
        }
        assert_eq!(call("PATCH", &entry, b"good"), 204);
        assert_eq!(call("POST", &entry, b""), 204);
        // A committed entry takes no more data, nor a second commit.
        assert_eq!(call("PATCH", &entry, b"evil"), 404);
        assert_eq!(call("POST", &entry, b""), 404);
        for mode in ["write-only/", "none/"] {
            assert_eq!(call("GET", &format!("{mode}{download}"), b""), 403);
        }
        assert_eq!(call("GET", &format!("read/{download}"), b""), 200);
        assert_eq!(
            std::fs::read(store.lock().unwrap().blob(id)).unwrap(),
            b"good"
        );
    }

    /// Downloads stream from the file instead of reading it into the response.
    #[test]
    fn cache_restore_streams_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(CacheStore::open(dir.path()).unwrap()));
        let blob = store.lock().unwrap().blob(9);
        std::fs::write(&blob, vec![7u8; 100_000]).unwrap();
        let resp = cache_handler(
            store,
            "tok",
            Request {
                method: "GET".into(),
                path: "/tok/_apis/artifactcache/artifacts/9".into(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: vec![],
            },
        );
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_empty() && resp.file.is_some());
        let mut wire = Vec::new();
        write_response(&mut wire, &resp).unwrap();
        let text = String::from_utf8_lossy(&wire);
        assert!(text.contains("Content-Length: 100000\r\n"));
        let body = &wire[wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
        assert_eq!(body, &vec![7u8; 100_000][..]);
    }

    /// v4 uploads: blocks joined into the artifact in blocklist order, its size
    /// recorded, and the download streamed back.
    #[test]
    fn artifact_blocks_join_and_download() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(ArtifactStore {
            dir: dir.path().to_path_buf(),
            artifacts: vec![],
            key: b"k".to_vec(),
        }));
        let call = |method: &str, path: &str, query: &str, body: &[u8]| {
            artifact_handler(
                store.clone(),
                Request {
                    method: method.into(),
                    path: path.into(),
                    query: query
                        .split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    headers: HashMap::new(),
                    body: body.to_vec(),
                },
            )
        };
        let twirp = |method: &str, body: Value| {
            let r = call(
                "POST",
                &format!("/twirp/github.actions.results.api.v1.ArtifactService/{method}"),
                "",
                body.to_string().as_bytes(),
            );
            serde_json::from_slice::<Value>(&r.body).unwrap()
        };
        for run in ["..", "/etc", "a/../..", "a/b", "C:", ""] {
            let r = twirp(
                "CreateArtifact",
                json!({"workflow_run_backend_id": run, "name": "out"}),
            );
            assert_eq!(r["code"], "invalid_argument", "{run}");
        }
        assert!(store.lock().unwrap().artifacts.is_empty());
        let ids = json!({"workflow_run_backend_id": "r", "name": "out"});
        let url = twirp("CreateArtifact", ids.clone())["signedUploadUrl"]
            .as_str()
            .unwrap()
            .to_string();
        let query = url.split_once('?').unwrap().1;
        for (block, data) in [("b1", b"hello ".as_slice()), ("b2", b"world")] {
            let q = format!("{query}&comp=block&blockid={block}");
            assert_eq!(call("PUT", "/upload/1", &q, data).status, 201);
        }
        let list = b"<BlockList><Latest>b2</Latest><Latest>b1</Latest></BlockList>";
        assert_eq!(
            call("PUT", "/upload/1", &format!("{query}&comp=blocklist"), list).status,
            201
        );
        assert_eq!(store.lock().unwrap().artifacts[0].size, 11);
        twirp("FinalizeArtifact", ids.clone());
        let url = twirp("GetSignedArtifactURL", ids)["signedUrl"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = call("GET", "/download/1", url.split_once('?').unwrap().1, b"");
        let mut wire = Vec::new();
        write_response(&mut wire, &resp).unwrap();
        assert!(wire.ends_with(b"\r\n\r\nworldhello "));
    }

    #[test]
    fn cache_lookup_prefers_exact_then_newest_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = CacheStore::open(dir.path()).unwrap();
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (id, key, created) in [(1, "deps-a", 1), (2, "deps-b", 3), (3, "deps-", 2)] {
            s.entries.push(CacheEntry {
                id,
                key: key.into(),
                version: "v".into(),
                complete: true,
                created,
                used: created,
            });
        }
        assert_eq!(s.lookup(&k(&["DEPS-A"]), "v").unwrap().id, 1);
        assert_eq!(s.lookup(&k(&["deps-a"]), "v").unwrap().id, 1);
        assert_eq!(s.lookup(&k(&["nope", "deps-"]), "v").unwrap().id, 3);
        assert_eq!(s.lookup(&k(&["deps"]), "v").unwrap().id, 2);
        assert!(s.lookup(&k(&["deps-a"]), "other").is_none());
    }

    #[test]
    fn gc_drops_stale_superseded_and_abandoned_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = CacheStore::open(dir.path()).unwrap();
        let now = 100 * DAY_MS;
        let entry = |id, key: &str, complete, created, used| CacheEntry {
            id,
            key: key.into(),
            version: "v".into(),
            complete,
            created,
            used,
        };
        s.entries = vec![
            entry(1, "old", true, now - 31 * DAY_MS, now - MINUTE_MS * 10), // created > 30d
            entry(2, "idle", true, now - 8 * DAY_MS, now - 8 * DAY_MS),     // unused > 7d
            entry(3, "k", true, now - DAY_MS, now - DAY_MS),                // superseded by 4
            entry(4, "k", true, now - MINUTE_MS * 30, now - MINUTE_MS * 30),
            entry(5, "up", false, now - MINUTE_MS * 10, now - MINUTE_MS * 10), // abandoned
            entry(6, "fresh", false, now - MINUTE_MS, now - MINUTE_MS),
        ];
        s.gc(now);
        let ids: Vec<u64> = s.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![4, 6]);
    }

    #[test]
    fn hmac_matches_rfc4231_and_urls_verify() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let store = ArtifactStore {
            dir: PathBuf::new(),
            artifacts: vec![],
            key: b"k".to_vec(),
        };
        let q: HashMap<String, String> = store
            .sign("upload", 7)
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(store.verify("upload", 7, &q));
        assert!(!store.verify("download", 7, &q));
        assert!(!store.verify("upload", 8, &q));
        let mut stale = q.clone();
        stale.insert("expires".into(), "1".into());
        assert!(!store.verify("upload", 7, &stale));
    }

    #[test]
    fn safe_relative_paths() {
        assert_eq!(safe_rel("a/b.txt"), Some(PathBuf::from("a/b.txt")));
        assert_eq!(safe_rel("/a//./b"), Some(PathBuf::from("a/b")));
        assert_eq!(safe_rel("a/../../etc"), None);
        assert_eq!(safe_rel(""), None);
    }

    #[test]
    fn timestamps_and_decoding() {
        assert_eq!(rfc3339_now().len(), 20);
        assert_eq!(percent_decode("a%2Bb%3D%3D"), "a+b==");
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        // A `%` before a multibyte character is kept, not sliced through.
        assert_eq!(percent_decode("%a\u{e9}"), "%a\u{e9}");
        assert_eq!(percent_decode("%\u{e9}1"), "%\u{e9}1");
    }

    #[test]
    fn a_body_is_allocated_as_it_arrives_not_as_announced() {
        let read = |raw: &str| read_request(&mut std::io::Cursor::new(raw.as_bytes().to_vec()));
        let req = read("PUT /x HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc")
            .unwrap()
            .unwrap();
        assert_eq!(req.body, b"abc");
        let req = read(
            "PUT /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n1\r\nc\r\n0\r\n\r\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(req.body, b"abc");
        // A length the client never sends is an error, not a 100 TB allocation.
        assert!(read("PUT /x HTTP/1.1\r\nContent-Length: 100000000000000\r\n\r\nabc").is_err());
        assert!(
            read("PUT /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffff\r\nabc")
                .is_err()
        );
    }

    #[test]
    fn runtime_token_grants_buildkit_a_writable_scope() {
        let servers = Servers {
            cache_port: Some(1),
            cache_token: "tok".into(),
            cache_external: None,
            artifact_port: None,
            run_id: "7".into(),
        };
        let env = servers.job_env("h", "build", CacheMode::Write);
        let token = &env
            .iter()
            .find(|(k, _)| k == "ACTIONS_RUNTIME_TOKEN")
            .unwrap()
            .1;
        let payload = token.split('.').nth(1).unwrap();
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bits = 0u32;
        let mut n = 0;
        let mut bytes = Vec::new();
        for c in payload.bytes() {
            bits = (bits << 6 & 0xffff) | alphabet.iter().position(|&a| a == c).unwrap() as u32;
            n += 6;
            if n >= 8 {
                n -= 8;
                bytes.push((bits >> n) as u8);
            }
        }
        let claims: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            claims["scp"],
            "Actions.ExampleScope Actions.Results:7:build"
        );
        let ac: Value = serde_json::from_str(claims["ac"].as_str().unwrap()).unwrap();
        assert_eq!(ac, json!([{"Scope": "refs/heads/main", "Permission": 3}]));
        assert!(claims["nbf"].is_number() && claims["exp"].is_number());
    }
}
