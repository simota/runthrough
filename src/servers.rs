//! Local stand-ins for the GitHub services actions talk to:
//!
//! - a cache server speaking the `@actions/cache` v1 API (`ACTIONS_CACHE_URL`) and the
//!   cache service v2 API (`ACTIONS_CACHE_SERVICE_V2`, `@actions/cache` 4 and later): Twirp
//!   JSON calls under `/twirp/github.actions.results.api.v1.CacheService/` at the root of
//!   `ACTIONS_RESULTS_URL`'s origin, plus Azure-blob-style uploads and plain downloads at
//!   signed `/cache-v2/` URLs; both APIs share one store, and v2 reads the job's
//!   `cache-mode` from its runtime token. With the artifact server on, `ACTIONS_RESULTS_URL`
//!   names the artifact port, which forwards the v2 routes to the cache server's handler; and
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
    /// Who reserved each pending v2 entry (the claims of its runtime token), so
    /// a finalize commits only the caller's own reservation. Not persisted: after
    /// a restart the signing keys are new, so those uploads cannot go on anyway.
    owners: HashMap<u64, String>,
}

/// Whether `e` was reserved, uploaded to or restored in the last 5 minutes:
/// a pending entry used longer ago is abandoned.
fn recently_used(e: &CacheEntry, now: u64) -> bool {
    now.saturating_sub(e.used.max(e.created)) < 5 * MINUTE_MS
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
            owners: HashMap::new(),
        };
        store.gc(now());
        Ok(store)
    }

    /// act's rules: drop uploads abandoned for 5 minutes, entries unused for 7
    /// days or created over 30 days ago, and older entries of the same key and
    /// version (unless used in the last 5 minutes).
    fn gc(&mut self, now: u64) {
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
                let recent = recently_used(e, now);
                let abandoned = !e.complete && !recent;
                let stale = now.saturating_sub(e.used.max(e.created)) > 7 * DAY_MS
                    || now.saturating_sub(e.created) > 30 * DAY_MS;
                let superseded = e.complete
                    && newest.get(&(e.key.clone(), e.version.clone())) != Some(&e.id)
                    && !recent;
                !(abandoned || stale || superseded)
            });
        for e in &drop {
            self.remove_files(e.id);
        }
        self.entries = keep;
        if !drop.is_empty() {
            self.save();
        }
    }

    /// Everything stored for entry `id`: its blob and, for a v2 upload that
    /// never finished, its staging file and blocks.
    fn remove_files(&mut self, id: u64) {
        let _ = std::fs::remove_file(self.blob(id));
        let _ = std::fs::remove_file(self.staging(id));
        let _ = std::fs::remove_dir_all(self.blocks(id));
        self.owners.remove(&id);
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(&self.entries) {
            let _ = std::fs::write(self.dir.join("index.json"), b);
        }
    }

    fn blob(&self, id: u64) -> PathBuf {
        self.dir.join("blobs").join(id.to_string())
    }

    /// Where a v2 upload's blocks wait for their block list, until the entry is
    /// finalized or dropped.
    fn blocks(&self, id: u64) -> PathBuf {
        self.dir.join("blocks").join(id.to_string())
    }

    /// Where a v2 upload lands (renamed into place by `CacheV2::place`); only
    /// a finalize publishes it as the blob, so a late upload cannot change a
    /// committed entry.
    fn staging(&self, id: u64) -> PathBuf {
        self.dir.join("blobs").join(format!("{id}.upload"))
    }

    /// A pending entry for `key` (lowercased) and `version`, recorded in
    /// `index.json` so a restart can still collect it; `None` when a complete
    /// one exists. Shared by the v1 and v2 APIs:
    ///
    /// - v1 (`owner` is `None`) replaces any pending entry of that key and
    ///   version, and its data goes straight into the (empty) blob;
    /// - v2 (`owner` is the caller's token claims) refuses while another
    ///   reservation of them is still in use, replaces only an abandoned one,
    ///   and also gets an empty staging file, which its finalize publishes
    ///   over the blob.
    ///
    /// A replaced entry's files go with it.
    fn reserve(&mut self, key: &str, version: &str, owner: Option<&str>) -> Option<u64> {
        let key = key.to_lowercase();
        let now = now();
        let same = |e: &CacheEntry| e.key == key && e.version == version;
        if self
            .entries
            .iter()
            .filter(|e| same(e))
            .any(|e| e.complete || (owner.is_some() && recently_used(e, now)))
        {
            return None;
        }
        let replaced: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| same(e))
            .map(|e| e.id)
            .collect();
        self.entries.retain(|e| !same(e));
        for old in replaced {
            self.remove_files(old);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(CacheEntry {
            id,
            key,
            version: version.to_string(),
            complete: false,
            created: now,
            used: now,
        });
        let _ = std::fs::write(self.blob(id), b"");
        if let Some(owner) = owner {
            let _ = std::fs::write(self.staging(id), b"");
            self.owners.insert(id, owner.to_string());
        }
        self.save();
        Some(id)
    }

    /// Mark the pending entry `id` complete; false when there is none.
    fn commit(&mut self, id: u64) -> bool {
        let Some(e) = self.entries.iter_mut().find(|e| e.id == id && !e.complete) else {
            return false;
        };
        e.complete = true;
        e.used = now();
        self.owners.remove(&id);
        let _ = std::fs::remove_dir_all(self.blocks(id));
        self.save();
        self.gc(now());
        true
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
            let key = body["key"].as_str().unwrap_or_default();
            let version = body["version"].as_str().unwrap_or_default();
            match store.reserve(key, version, None) {
                Some(id) => Response::json(201, json!({"cacheId": id})),
                None => Response::json(409, json!({"message": "cache already exists"})),
            }
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
            if store.commit(id) {
                Response::empty(204)
            } else {
                Response::empty(404)
            }
        }
        ("GET", ["artifacts", id]) => match id.parse::<u64>().map(|id| store.blob(id)) {
            Ok(p) => Response::file(&p, "application/octet-stream", None),
            Err(_) => Response::empty(400),
        },
        _ => Response::empty(404),
    }
}

// ------------------------------------------------------------------ cache (v2 API)

/// Twirp prefix of the cache service v2 API.
const CACHE_SERVICE: &str = "/twirp/github.actions.results.api.v1.CacheService/";
/// Twirp prefix of the v4 artifact API.
const ARTIFACT_SERVICE: &str = "/twirp/github.actions.results.api.v1.ArtifactService/";

/// State of the cache service v2 API, served by the cache server and, with the
/// artifact server on, forwarded from it.
#[derive(Clone)]
struct CacheV2 {
    /// The same store the v1 API uses.
    store: Arc<Mutex<CacheStore>>,
    /// Signs the `/cache-v2/` blob URLs; not the artifact server's key.
    signer: UrlSigner,
    auth: RuntimeAuth,
    /// The origin of `--cache-server-external-url`: jobs reach the cache port
    /// there, so its signed blob URLs name it instead of `http://<Host>`.
    external: Option<String>,
}

/// The cache server: v2 routes, a pointer to `--artifact-server-path` for
/// artifact calls (`ACTIONS_RESULTS_URL` names this port when the artifact
/// server is off), and the v1 API under the per-run `token` prefix.
fn cache_server(v2: &CacheV2, token: &str, req: Request) -> Response {
    let origin = match &v2.external {
        Some(o) => o.clone(),
        None => format!("http://{}", host_header(&req)),
    };
    if let Some(r) = cache_v2_route(v2, &req, &origin) {
        return r;
    }
    if req.path.starts_with(ARTIFACT_SERVICE) {
        return twirp_error(
            404,
            "bad_route",
            "the artifact server is off: run with --artifact-server-path",
        );
    }
    cache_handler(v2.store.clone(), token, req)
}

/// The request's `Host` header, or empty.
fn host_header(req: &Request) -> String {
    req.headers.get("host").cloned().unwrap_or_default()
}

/// The v2 routes (`/twirp/…CacheService/<method>`, `/cache-v2/…`); `None` for
/// any other path. `origin` is where the job reaches this server, for the
/// signed blob URLs.
fn cache_v2_route(v2: &CacheV2, req: &Request, origin: &str) -> Option<Response> {
    if let Some(method) = req.path.strip_prefix(CACHE_SERVICE) {
        return Some(cache_v2_twirp(v2, method, req, origin));
    }
    req.path
        .starts_with("/cache-v2/")
        .then(|| cache_v2_blob(v2, req))
}

/// The cache service v2 Twirp calls. Every answer has a JSON body: the toolkit
/// retries any other for about 20 seconds. Responses use the proto field
/// names: the toolkit masks the signatures in `signed_upload_url` and
/// `signed_download_url` only, before it logs the body at debug level. No
/// message repeats the key: the toolkit takes any `msg` containing
/// `insufficient usage` for a quota error.
fn cache_v2_twirp(v2: &CacheV2, method: &str, req: &Request, origin: &str) -> Response {
    let Some(claims) = v2.auth.claims(req) else {
        return twirp_error(
            401,
            "unauthenticated",
            "missing or invalid ACTIONS_RUNTIME_TOKEN",
        );
    };
    let known = [
        "CreateCacheEntry",
        "FinalizeCacheEntryUpload",
        "GetCacheEntryDownloadURL",
    ];
    if req.method != "POST" || !known.contains(&method) {
        return twirp_error(404, "bad_route", method);
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&req.body) else {
        return twirp_error(400, "malformed", "the request body is not a JSON object");
    };
    // The path cannot carry the job's cache-mode (the toolkit drops it), so the
    // token does; the caller's own token has no such claim and means `write`.
    let mode = claims["runthrough_cache_mode"]
        .as_str()
        .and_then(CacheMode::parse)
        .unwrap_or(CacheMode::Write);
    // The job a reservation belongs to: tokens this run signs name it in `scp`.
    let owner = claims.to_string();
    // The toolkit sends proto field names; protobuf JSON also allows lowerCamelCase.
    let field = |camel: &str, snake: &str| -> Value {
        body.get(snake)
            .or_else(|| body.get(camel))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let text = |name: &str| body.get(name).and_then(Value::as_str).unwrap_or_default();
    let key = text("key").to_lowercase();
    let version = text("version");
    // A refused save is `ok: false` with a reason, which the toolkit logs as a
    // warning without failing the step.
    let refused = |message: String| Response::json(200, json!({"ok": false, "message": message}));
    let save_denied = || refused(format!("cache-mode {} does not allow saving", mode.name()));
    match method {
        "CreateCacheEntry" => {
            if !mode.can_save() {
                return save_denied();
            }
            let Some(id) = v2
                .store
                .lock()
                .unwrap()
                .reserve(&key, version, Some(&owner))
            else {
                // Non-retryable: logged as "another job may be creating this cache".
                return twirp_error(
                    409,
                    "already_exists",
                    "a cache entry with this key and version exists or is being uploaded",
                );
            };
            Response::json(
                200,
                json!({"ok": true, "signed_upload_url": format!(
                    "{origin}/cache-v2/upload/{id}?{}", v2.signer.sign("cache-upload", id)
                )}),
            )
        }
        "FinalizeCacheEntryUpload" => {
            if !mode.can_save() {
                return save_denied();
            }
            // Left out when 0 (`emitDefaultValues: false`).
            let size = match field("sizeBytes", "size_bytes") {
                Value::Null => Some(0),
                v => json_u64(&v),
            };
            let Some(size) = size else {
                return refused("size_bytes is not a valid size".into());
            };
            let mut store = v2.store.lock().unwrap();
            // Only the reservation this job made: another job's upload of the
            // same key is not what this job sent.
            let Some(id) = store
                .entries
                .iter()
                .find(|e| {
                    !e.complete
                        && e.key == key
                        && e.version == version
                        && store.owners.get(&e.id) == Some(&owner)
                })
                .map(|e| e.id)
            else {
                return refused("no pending upload for this key and version".into());
            };
            let staging = store.staging(id);
            let Ok(uploaded) = std::fs::metadata(&staging).map(|m| m.len()) else {
                return refused("the uploaded data is missing".into());
            };
            if uploaded != size {
                // Stays pending; `gc` drops it after 5 minutes.
                return refused(format!("uploaded {uploaded} bytes, expected {size}"));
            }
            if std::fs::rename(&staging, store.blob(id)).is_err() {
                return refused("the uploaded data could not be stored".into());
            }
            store.commit(id);
            Response::json(200, json!({"ok": true, "entry_id": id.to_string()}))
        }
        "GetCacheEntryDownloadURL" => {
            // A miss is `ok: false`, logged by the toolkit at debug level.
            let miss = || Response::json(200, json!({"ok": false}));
            if !mode.can_restore() {
                return miss();
            }
            let restore_keys = field("restoreKeys", "restore_keys");
            // An empty key would prefix-match every entry.
            let keys: Vec<String> = std::iter::once(key)
                .chain(
                    restore_keys
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|k| k.as_str().map(String::from)),
                )
                .filter(|k| !k.is_empty())
                .collect();
            let mut store = v2.store.lock().unwrap();
            let Some((id, matched)) = store.lookup(&keys, version).map(|e| (e.id, e.key.clone()))
            else {
                return miss();
            };
            if let Some(e) = store.entries.iter_mut().find(|e| e.id == id) {
                e.used = now();
            }
            store.save();
            Response::json(
                200,
                json!({
                    "ok": true, "matched_key": matched,
                    "signed_download_url": format!(
                        "{origin}/cache-v2/download/{id}?{}", v2.signer.sign("cache-download", id)
                    ),
                }),
            )
        }
        _ => unreachable!("checked against `known`"),
    }
}

impl CacheV2 {
    /// Put `staged`, received for entry `id` without the lock, in place while
    /// `id` is still pending: a PUT that loses a race with a finalize or `gc`
    /// leaves the committed blob alone and recreates nothing (the staged file
    /// goes when it drops). Only renames happen under the lock. A placed upload
    /// counts as use, so `gc` keeps a reservation whose data still arrives.
    fn place(&self, id: u64, staged: Staged) -> Response {
        let mut store = self.store.lock().unwrap();
        let (blocks, staging) = (store.blocks(id), store.staging(id));
        let Some(e) = store.entries.iter_mut().find(|e| e.id == id && !e.complete) else {
            return Response::empty(404);
        };
        match staged.place(&blocks, &staging) {
            Ok(_) => {
                e.used = now();
                Response::empty(201)
            }
            Err(_) => Response::empty(400),
        }
    }
}

/// Uploads (Put Blob, or Put Block and Put Block List) to a pending entry and
/// plain downloads of a complete one, at the URLs `cache_v2_twirp` signs. The
/// signature alone authorizes them: only calls the job's cache-mode allows
/// hand them out.
fn cache_v2_blob(v2: &CacheV2, req: &Request) -> Response {
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    let (upload, id) = match (req.method.as_str(), parts.as_slice()) {
        ("PUT", ["cache-v2", "upload", id]) => (true, id),
        ("GET", ["cache-v2", "download", id]) => (false, id),
        _ => return Response::empty(404),
    };
    let Ok(id) = id.parse::<u64>() else {
        return Response::empty(400);
    };
    let kind = if upload {
        "cache-upload"
    } else {
        "cache-download"
    };
    if !v2.signer.verify(kind, id, &req.query) {
        return Response::empty(401);
    }
    let store = v2.store.lock().unwrap();
    // Uploads go to a pending entry only, downloads come from a complete one.
    if !store
        .entries
        .iter()
        .any(|e| e.id == id && e.complete != upload)
    {
        return Response::empty(404);
    }
    let (blob, blocks, staging) = (store.blob(id), store.blocks(id), store.staging(id));
    // Other jobs' cache calls go on while blob data is copied.
    drop(store);
    if !upload {
        return Response::file(&blob, "application/octet-stream", None);
    }
    match stage_block_blob(req, &blocks, &staging) {
        Ok(staged) => v2.place(id, staged),
        Err(_) => Response::empty(400),
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
    /// Signs the v4 upload/download URLs with a per-run key.
    signer: UrlSigner,
    /// Which bearer tokens the Twirp and v3 calls must carry.
    auth: RuntimeAuth,
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

/// `a == b` in time that does not depend on where they first differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// The `ACTIONS_RUNTIME_TOKEN`s of this run: JWTs signed (HS256) with a
/// per-run key, or the one taken from the caller's environment. The artifact
/// API accepts only requests that carry one, as GitHub's does, so other local
/// users (or the LAN, where the servers listen beyond loopback) cannot read
/// or replace a run's artifacts.
#[derive(Clone)]
struct RuntimeAuth {
    key: Vec<u8>,
    from_env: Option<String>,
}

impl RuntimeAuth {
    fn new(from_env: Option<String>) -> RuntimeAuth {
        RuntimeAuth {
            key: hex::decode(random_token()).unwrap_or_default(),
            from_env,
        }
    }

    /// The token jobs get: the caller's, else one this run signs over `claims`.
    fn issue(&self, claims: &Value) -> String {
        if let Some(t) = &self.from_env {
            return t.clone();
        }
        let signed = format!(
            "{}.{}",
            base64url(br#"{"alg":"HS256","typ":"JWT"}"#),
            base64url(claims.to_string().as_bytes())
        );
        let sig = base64url(&hmac_sha256(&self.key, signed.as_bytes()));
        format!("{signed}.{sig}")
    }

    /// Whether `req` carries `Authorization: Bearer <a token this run issued>`.
    fn accepts(&self, req: &Request) -> bool {
        self.claims(req).is_some()
    }

    /// The claims of the token `req` carries: `None` when `accepts` refuses it,
    /// the decoded payload of a token this run signed, and `Null` for the
    /// caller's token, whose claims are neither readable nor trusted here.
    fn claims(&self, req: &Request) -> Option<Value> {
        let token = req
            .headers
            .get("authorization")
            .and_then(|h| h.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t.trim())?;
        if let Some(t) = &self.from_env {
            return same(token, t).then_some(Value::Null);
        }
        let (signed, sig) = token.rsplit_once('.')?;
        if !same(sig, &base64url(&hmac_sha256(&self.key, signed.as_bytes()))) {
            return None;
        }
        let (_, payload) = signed.split_once('.')?;
        serde_json::from_slice(&base64url_decode(payload)?).ok()
    }
}

/// Signs URLs that authorize one blob operation for `SIGNED_URL_TTL_MS`: HMAC-SHA256
/// over `<kind>/<id>/<expires>` with a per-run key.
#[derive(Clone)]
struct UrlSigner {
    key: Vec<u8>,
}

impl UrlSigner {
    fn new() -> UrlSigner {
        UrlSigner {
            key: hex::decode(random_token()).unwrap_or_default(),
        }
    }

    /// `expires=…&sig=…` for `/{kind}/{id}`.
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
        fresh && same(&want, sig)
    }
}

/// An Azure-blob-style upload received into a temporary file beside the file it
/// is for, so that a failed, repeated or late request never truncates what is
/// there; `Staged::place` renames it into place.
enum Staged {
    /// Put Block (`comp=block`): the block, for `blocks/<hex(blockid)>`.
    Block(tempfile::NamedTempFile, String),
    /// Put Blob (no `comp`), or Put Block List (`comp=blocklist`) with the
    /// listed blocks joined in order: the whole blob, and its size.
    Blob(tempfile::NamedTempFile, u64),
}

impl Staged {
    /// Rename into place: a block under `blocks`, a blob over `file`. The
    /// blob's size; `None` for a block.
    fn place(self, blocks: &Path, file: &Path) -> Result<Option<u64>> {
        match self {
            Staged::Block(tmp, name) => {
                std::fs::create_dir_all(blocks)?;
                tmp.persist(blocks.join(name))?;
                Ok(None)
            }
            Staged::Blob(tmp, size) => {
                tmp.persist(file)?;
                Ok(Some(size))
            }
        }
    }
}

/// Receive an Azure-blob-style upload of `req` for `file` (blocks under
/// `blocks`) into a temporary file in `file`'s directory. A block list reads
/// the blocks and leaves them, so the same list sent again (a retry whose
/// first answer got lost) joins the same blob; whoever owns the upload removes
/// `blocks` when it is finalized.
fn stage_block_blob(req: &Request, blocks: &Path, file: &Path) -> Result<Staged> {
    let dir = file.parent().context("upload path without a directory")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    Ok(match req.query.get("comp").map(String::as_str) {
        Some("block") => {
            let block = req.query.get("blockid").cloned().unwrap_or_default();
            tmp.write_all(&req.body)?;
            Staged::Block(tmp, hex::encode(block))
        }
        Some("blocklist") => {
            let xml = String::from_utf8_lossy(&req.body);
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
                size += std::io::copy(&mut data, tmp.as_file_mut())?;
            }
            Staged::Blob(tmp, size)
        }
        _ => {
            tmp.write_all(&req.body)?;
            Staged::Blob(tmp, req.body.len() as u64)
        }
    })
}

/// An Azure-blob-style upload of `req` to `file`, staged and then put in
/// place at once (see `stage_block_blob`). Returns the size of `file` when it
/// was written.
fn write_block_blob(req: &Request, blocks: &Path, file: &Path) -> Result<Option<u64>> {
    stage_block_blob(req, blocks, file)?.place(blocks, file)
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

/// The artifact server; with the cache server on (`cache_v2`), it also serves
/// the cache v2 routes, as both share `ACTIONS_RESULTS_URL`.
fn artifact_handler(
    shared: Arc<Mutex<ArtifactStore>>,
    cache_v2: Option<&CacheV2>,
    req: Request,
) -> Response {
    let host = host_header(&req);
    // The cache handler checks tokens and signatures itself. Jobs reach this
    // port at its `Host`, also with `--cache-server-external-url`.
    if let Some(r) = cache_v2.and_then(|v2| cache_v2_route(v2, &req, &format!("http://{host}"))) {
        return r;
    }
    let mut store = shared.lock().unwrap();
    // Block uploads and downloads go to the signed URLs `twirp` hands out,
    // without a token; every other call carries the job's.
    let signed = req.path.starts_with("/upload/") || req.path.starts_with("/download/");
    if !signed && !store.auth.accepts(&req) {
        return Response::json(
            401,
            json!({"code": "unauthenticated", "msg": "missing or invalid ACTIONS_RUNTIME_TOKEN"}),
        );
    }
    if req.path.starts_with("/_apis/pipelines/") || req.path.starts_with("/v3/") {
        // The v3 API keeps no state beyond its files.
        let dir = store.dir.clone();
        drop(store);
        return artifact_v3(&dir, &host, &req);
    }
    match req.path.strip_prefix(ARTIFACT_SERVICE) {
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
                json!({"ok": true, "signedUploadUrl": format!("http://{host}/upload/{id}?{}", store.signer.sign("upload", id))}),
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
                    let id = a.id;
                    // A block list leaves its blocks for a retry of it.
                    let _ = std::fs::remove_dir_all(store.blocks_dir(id));
                    Response::json(200, json!({"ok": true, "artifactId": id.to_string()}))
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
                json!({"signedUrl": format!("http://{host}/download/{}?{}", store.artifacts[i].id, store.signer.sign("download", store.artifacts[i].id))}),
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
            if !store.signer.verify("upload", id, &req.query) {
                return Response::empty(401);
            }
            let Some(idx) = store.artifacts.iter().position(|a| a.id == id) else {
                return Response::empty(404);
            };
            let blocks = store.blocks_dir(id);
            let file = store.file(&store.artifacts[idx]);
            // Blocks are written and joined while other jobs' calls go on; only
            // the size is set under the lock.
            drop(store);
            let result = write_block_blob(req, &blocks, &file);
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
            if !store.signer.verify("download", id, &req.query) {
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
    auth: RuntimeAuth,
}

fn base64url(data: &[u8]) -> String {
    base64(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// Unpadded base64url, as in a JWT; `None` on any other character.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut bits, mut n) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = (bits << 6 | u32::from(v)) & 0xffff;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    Some(out)
}

/// The scheme and authority of `url`, without path, query or fragment.
fn url_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{authority}")
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
            // As in act, a token from the caller's environment wins.
            auth: RuntimeAuth::new(
                std::env::var("ACTIONS_RUNTIME_TOKEN")
                    .ok()
                    .filter(|v| !v.is_empty()),
            ),
        };
        // Set when the cache server runs, for the artifact server to forward to.
        let mut cache_v2 = None;
        if !cli.no_cache_server {
            let dir = cli
                .cache_server_path
                .clone()
                .unwrap_or_else(|| base.join("cache"));
            let started = CacheStore::open(&dir).and_then(|store| {
                let v2 = CacheV2 {
                    store: Arc::new(Mutex::new(store)),
                    signer: UrlSigner::new(),
                    auth: servers.auth.clone(),
                    external: cli.cache_server_external_url.as_deref().map(url_origin),
                };
                let token = servers.cache_token.clone();
                let handler = v2.clone();
                let port = serve(
                    cli.cache_server_addr.as_deref().unwrap_or(default_addr),
                    cli.cache_server_port,
                    Arc::new(move |req| cache_server(&handler, &token, req)),
                )?;
                Ok((port, v2))
            });
            match started {
                Ok((p, v2)) => {
                    servers.cache_port = Some(p);
                    cache_v2 = Some(v2);
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
                signer: UrlSigner::new(),
                auth: servers.auth.clone(),
            }));
            let p = serve(
                cli.artifact_server_addr.as_deref().unwrap_or(default_addr),
                cli.artifact_server_port,
                Arc::new(move |req| artifact_handler(store.clone(), cache_v2.as_ref(), req)),
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
    /// `cache_mode` is the job's `cache-mode` (`read`, `write`, `write-only`, `none`):
    /// in `ACTIONS_CACHE_URL`'s path for the v1 API, and in the runtime token's
    /// `runthrough_cache_mode` claim for the v2 API, whose client drops the path of
    /// `ACTIONS_RESULTS_URL`.
    pub fn job_env(&self, host: &str, job: &str, cache_mode: CacheMode) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if self.cache_port.is_none() && self.artifact_port.is_none() {
            return env;
        }
        // BuildKit's `type=gha` cache refuses a token without `ac`, `exp` or `nbf`.
        // `ac` (a JSON string, as GitHub sends it) grants read and write on the
        // scope the cache server reports; BuildKit writes only to a writable one.
        let claims = json!({
            "scp": format!("Actions.ExampleScope Actions.Results:{}:{job}", self.run_id),
            "ac": json!([{"Scope": "refs/heads/main", "Permission": 3}]).to_string(),
            "nbf": 0,
            "exp": 4_102_444_800u64,
            "runthrough_cache_mode": cache_mode.name(),
        });
        env.push(("ACTIONS_RUNTIME_TOKEN".into(), self.auth.issue(&claims)));
        // As in act, values from the caller's environment win.
        let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(p) = self.cache_port {
            let path = format!("{}/{}", self.cache_token, cache_mode_segment(cache_mode));
            let url = match &self.cache_external {
                Some(u) => format!("{u}/{path}"),
                None => format!("http://{host}:{p}/{path}"),
            };
            env.push(("ACTIONS_CACHE_URL".into(), url));
            // The caller's token is the same for every job and carries no
            // cache-mode, which v2 then takes as `write`: other jobs keep v1,
            // which enforces their mode by URL. They get an empty value, not
            // none, so a host job cannot inherit v2 from runthrough's own
            // environment (the toolkit tests the variable for truthiness).
            let v2 = self.auth.from_env.is_none() || cache_mode == CacheMode::Write;
            env.push((
                "ACTIONS_CACHE_SERVICE_V2".into(),
                if v2 { "true" } else { "" }.into(),
            ));
            // v2 is served at the root of this origin (the toolkit drops any
            // path); with the artifact server on, its port forwards v2 calls.
            if self.artifact_port.is_none() {
                let origin = match &self.cache_external {
                    Some(u) => url_origin(u),
                    None => format!("http://{host}:{p}"),
                };
                env.push(("ACTIONS_RESULTS_URL".into(), format!("{origin}/")));
            }
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
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({"scp": "x"}));
        let store = Arc::new(Mutex::new(ArtifactStore {
            dir: dir.path().to_path_buf(),
            artifacts: vec![],
            signer: UrlSigner { key: b"k".to_vec() },
            auth,
        }));
        let call = |method: &str, path: &str, query: &str, body: &[u8]| {
            artifact_handler(
                store.clone(),
                None,
                Request {
                    method: method.into(),
                    path: path.into(),
                    query: query
                        .split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    // Signed URLs need no token; the toolkit sends it anyway on API calls.
                    headers: HashMap::from([(
                        "authorization".to_string(),
                        format!("Bearer {token}"),
                    )]),
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

    /// Without a token this run issued, the artifact API refuses every call
    /// but the signed block URLs, whose signature is checked instead.
    #[test]
    fn artifact_api_calls_need_a_runtime_token_of_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({"scp": "Actions.Results:1:j"}));
        let store = Arc::new(Mutex::new(ArtifactStore {
            dir: dir.path().to_path_buf(),
            artifacts: vec![],
            signer: UrlSigner { key: b"k".to_vec() },
            auth,
        }));
        let call = |method: &str, path: &str, auth: Option<&str>| {
            artifact_handler(
                store.clone(),
                None,
                Request {
                    method: method.into(),
                    path: path.into(),
                    query: HashMap::from([("itemPath".into(), "a/f".into())]),
                    headers: auth
                        .map(|a| HashMap::from([("authorization".to_string(), a.to_string())]))
                        .unwrap_or_default(),
                    body: json!({"workflow_run_backend_id": "r", "name": "a"})
                        .to_string()
                        .into_bytes(),
                },
            )
            .status
        };
        let create = "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact";
        let list = "/twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts";
        let (head, payload) = token.rsplit_once('.').unwrap().0.split_once('.').unwrap();
        let forged = format!("{head}.{payload}.runthrough");
        let other_run = RuntimeAuth::new(None).issue(&json!({"scp": "Actions.Results:1:j"}));
        for bad in [
            None,
            Some(forged.as_str()),
            Some(other_run.as_str()),
            Some(token.as_str()),
        ] {
            let header = bad.map(|t| {
                if t == token {
                    format!("Basic {t}")
                } else {
                    format!("Bearer {t}")
                }
            });
            for (method, path) in [
                ("POST", create),
                ("POST", list),
                ("GET", "/_apis/pipelines/workflows/r/artifacts"),
                ("PUT", "/v3/upload/r"),
                ("GET", "/v3/download/r"),
            ] {
                assert_eq!(
                    call(method, path, header.as_deref()),
                    401,
                    "{path} {header:?}"
                );
            }
        }
        assert!(!dir.path().join("v3").exists());
        let bearer = format!("bearer {token}");
        assert_eq!(call("POST", create, Some(&bearer)), 200);
        assert_eq!(call("POST", list, Some(&bearer)), 200);
        // A block URL is judged by its signature alone.
        assert_eq!(call("PUT", "/upload/1", None), 401);
        assert_eq!(call("GET", "/download/1", Some(&bearer)), 401);
    }

    #[test]
    fn a_token_from_the_callers_environment_is_issued_and_accepted() {
        let auth = RuntimeAuth::new(Some("ghs_real".into()));
        assert_eq!(auth.issue(&json!({})), "ghs_real");
        let req = |h: &str| Request {
            method: "POST".into(),
            path: "/".into(),
            query: HashMap::new(),
            headers: HashMap::from([("authorization".to_string(), h.to_string())]),
            body: vec![],
        };
        assert!(auth.accepts(&req("Bearer ghs_real")));
        assert!(!auth.accepts(&req("Bearer ghs_other")));
        let signed = RuntimeAuth {
            key: auth.key.clone(),
            from_env: None,
        }
        .issue(&json!({}));
        assert!(!auth.accepts(&req(&format!("Bearer {signed}"))));
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
        let store = UrlSigner { key: b"k".to_vec() };
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
            auth: RuntimeAuth::new(None),
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

    // ---------------------------------------------------------- cache v2

    /// A cache v2 service over a store in `dir`.
    fn v2_service(dir: &Path, auth: RuntimeAuth) -> CacheV2 {
        CacheV2 {
            store: Arc::new(Mutex::new(CacheStore::open(dir).unwrap())),
            signer: UrlSigner::new(),
            auth,
            external: None,
        }
    }

    /// A Twirp `CacheService` call with a raw body to `host: h:1`; its status
    /// and JSON body.
    fn v2_twirp_raw(
        handler: &dyn Fn(Request) -> Response,
        token: Option<&str>,
        method: &str,
        body: &[u8],
    ) -> (u16, Value) {
        let mut headers = HashMap::from([("host".to_string(), "h:1".to_string())]);
        if let Some(t) = token {
            headers.insert("authorization".into(), format!("Bearer {t}"));
        }
        let r = handler(Request {
            method: "POST".into(),
            path: format!("{CACHE_SERVICE}{method}"),
            query: HashMap::new(),
            headers,
            body: body.to_vec(),
        });
        (r.status, serde_json::from_slice(&r.body).unwrap())
    }

    /// A Twirp `CacheService` call to `host: h:1`; its status and JSON body.
    fn v2_twirp(
        handler: &dyn Fn(Request) -> Response,
        token: Option<&str>,
        method: &str,
        body: Value,
    ) -> (u16, Value) {
        v2_twirp_raw(handler, token, method, body.to_string().as_bytes())
    }

    /// A request to `url` (any origin) with `extra` appended to its query, as
    /// the Azure SDK does.
    fn url_req(
        method: &str,
        url: &str,
        extra: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Request {
        let rest = url.split_once("://").unwrap().1;
        let rest = &rest[rest.find('/').unwrap()..];
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        Request {
            method: method.into(),
            path: path.into(),
            query: query
                .split('&')
                .chain(extra.split('&'))
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    /// The response as sent: its header block and its body.
    fn on_wire(resp: &Response) -> (String, Vec<u8>) {
        let mut wire = Vec::new();
        write_response(&mut wire, resp).unwrap();
        let end = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8_lossy(&wire[..end]).into_owned(),
            wire[end..].to_vec(),
        )
    }

    /// The field names of a JSON object, sorted.
    fn fields(v: &Value) -> Vec<&str> {
        let mut names: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        names.sort();
        names
    }

    /// CreateCacheEntry that succeeds; the signed upload URL.
    fn v2_create(h: &dyn Fn(Request) -> Response, token: &str, key: &str) -> String {
        let (status, created) = v2_twirp(
            h,
            Some(token),
            "CreateCacheEntry",
            json!({"key": key, "version": "v"}),
        );
        assert_eq!(status, 200);
        assert_eq!(fields(&created), ["ok", "signed_upload_url"], "{created}");
        assert_eq!(created["ok"], true);
        created["signed_upload_url"].as_str().unwrap().to_string()
    }

    /// FinalizeCacheEntryUpload of `size` bytes; its status-200 body.
    fn v2_finalize(h: &dyn Fn(Request) -> Response, token: &str, key: &str, size: Value) -> Value {
        let (status, done) = v2_twirp(
            h,
            Some(token),
            "FinalizeCacheEntryUpload",
            json!({"key": key, "version": "v", "size_bytes": size}),
        );
        assert_eq!(status, 200);
        done
    }

    /// Create → Put Blob → Finalize; the signed upload URL.
    fn v2_save(h: &dyn Fn(Request) -> Response, token: &str, key: &str, data: &[u8]) -> String {
        let url = v2_create(h, token, key);
        let put = h(url_req(
            "PUT",
            &url,
            "",
            &[("x-ms-blob-type", "BlockBlob")],
            data,
        ));
        assert_eq!(put.status, 201);
        let done = v2_finalize(h, token, key, json!(data.len().to_string()));
        assert_eq!(fields(&done), ["entry_id", "ok"], "{done}");
        assert_eq!(done["ok"], true);
        url
    }

    /// GetCacheEntryDownloadURL then GET: the matched key and the bytes, or
    /// `None` for a miss, which must be exactly `{"ok": false}` with status 200.
    fn v2_restore(
        h: &dyn Fn(Request) -> Response,
        token: &str,
        key: &str,
        restore_keys: &[&str],
    ) -> Option<(String, Vec<u8>)> {
        let (status, found) = v2_twirp(
            h,
            Some(token),
            "GetCacheEntryDownloadURL",
            json!({"key": key, "restore_keys": restore_keys, "version": "v"}),
        );
        assert_eq!(status, 200);
        if found == json!({"ok": false}) {
            return None;
        }
        assert_eq!(
            fields(&found),
            ["matched_key", "ok", "signed_download_url"],
            "{found}"
        );
        assert_eq!(found["ok"], true);
        let resp = h(url_req(
            "GET",
            found["signed_download_url"].as_str().unwrap(),
            "",
            &[],
            b"",
        ));
        assert_eq!(resp.status, 200);
        let (head, body) = on_wire(&resp);
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
        Some((found["matched_key"].as_str().unwrap().to_string(), body))
    }

    /// Entry `id` last reserved or used `minutes` ago.
    fn age(v2: &CacheV2, id: u64, minutes: u64) {
        for e in v2
            .store
            .lock()
            .unwrap()
            .entries
            .iter_mut()
            .filter(|e| e.id == id)
        {
            e.created = now() - minutes * MINUTE_MS;
            e.used = e.created;
        }
    }

    /// The names in the store's `blobs/`, sorted: no staged upload is left over.
    fn blob_files(v2: &CacheV2) -> Vec<String> {
        let dir = v2.store.lock().unwrap().dir.join("blobs");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn cache_v2_save_then_restore_returns_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_create(&h, &token, "Deps-Linux");
        assert!(
            url.starts_with("http://h:1/cache-v2/upload/1?expires="),
            "{url}"
        );
        let put = h(url_req(
            "PUT",
            &url,
            "",
            &[("x-ms-blob-type", "BlockBlob")],
            b"cached bytes",
        ));
        assert_eq!(put.status, 201);
        assert_eq!(
            v2_finalize(&h, &token, "Deps-Linux", json!("12")),
            json!({"ok": true, "entry_id": "1"})
        );
        assert_eq!(
            v2_restore(&h, &token, "Deps-Linux", &[]),
            Some(("deps-linux".to_string(), b"cached bytes".to_vec()))
        );
        // A second save of the same key and version is a non-retryable conflict.
        let (status, again) = v2_twirp(
            &h,
            Some(&token),
            "CreateCacheEntry",
            json!({"key": "deps-linux", "version": "v"}),
        );
        assert_eq!(
            (status, again),
            (
                409,
                json!({"code": "already_exists", "msg": "a cache entry with this key and version exists or is being uploaded"})
            )
        );
        let (status, unknown) = v2_twirp(&h, Some(&token), "DeleteCacheEntry", json!({}));
        assert_eq!((status, unknown["code"].as_str()), (404, Some("bad_route")));
    }

    /// The toolkit masks the signatures of `signed_upload_url` and
    /// `signed_download_url` (proto names) before it logs a response body, so
    /// every answer uses proto names and none the lowerCamelCase ones.
    #[test]
    fn cache_v2_answers_with_proto_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        // `v2_create`, `v2_save` and `v2_restore` check the exact field names.
        v2_save(&h, &token, "k", b"data");
        assert!(v2_restore(&h, &token, "k", &[]).is_some());
        let (_, created) = v2_twirp(
            &h,
            Some(&token),
            "CreateCacheEntry",
            json!({"key": "other", "version": "v"}),
        );
        let (_, done) = v2_twirp(
            &h,
            Some(&token),
            "FinalizeCacheEntryUpload",
            json!({"key": "other", "version": "v", "size_bytes": "9"}),
        );
        assert_eq!(fields(&done), ["message", "ok"]);
        for body in [created, done] {
            for camel in [
                "signedUploadUrl",
                "signedDownloadUrl",
                "entryId",
                "matchedKey",
            ] {
                assert!(body.get(camel).is_none(), "{body}");
            }
        }
    }

    #[test]
    fn cache_v2_block_upload_joins_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_create(&h, &token, "big");
        for (block, data) in [("AAAB", b"world".as_slice()), ("AAAA", b"hello ")] {
            let extra = format!("comp=block&blockid={block}");
            assert_eq!(h(url_req("PUT", &url, &extra, &[], data)).status, 201);
        }
        let blocks = dir.path().join("blocks").join("1");
        assert!(blocks.is_dir());
        let list = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><BlockList><Latest>AAAA</Latest><Latest>AAAB</Latest></BlockList>"#;
        assert_eq!(
            h(url_req("PUT", &url, "comp=blocklist", &[], list)).status,
            201
        );
        let done = v2_finalize(&h, &token, "big", json!(11));
        assert_eq!(done["ok"], true);
        // The blocks stay for a repeated block list until the entry is final.
        assert!(!blocks.exists());
        assert_eq!(
            v2_restore(&h, &token, "big", &[]).unwrap().1,
            b"hello world"
        );
        assert_eq!(blob_files(&v2), ["1"]);
    }

    /// A Put Block List sent again after it succeeded (its 201 lost on the way)
    /// answers 201 with the same blob; one naming a missing block fails and
    /// leaves what was there. Shared with the artifact server.
    #[test]
    fn block_lists_are_atomic_and_can_be_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let (blocks, file) = (dir.path().join("blocks"), dir.path().join("out").join("f"));
        let put = |query: &str, body: &[u8]| {
            write_block_blob(
                &url_req("PUT", &format!("http://h:1/x?{query}"), "", &[], body),
                &blocks,
                &file,
            )
        };
        put("comp=block&blockid=b1", b"hello ").unwrap();
        put("comp=block&blockid=b2", b"world").unwrap();
        let list = b"<BlockList><Latest>b1</Latest><Latest>b2</Latest></BlockList>";
        for _ in 0..2 {
            assert_eq!(put("comp=blocklist", list).unwrap(), Some(11));
            assert_eq!(std::fs::read(&file).unwrap(), b"hello world");
        }
        let broken = b"<BlockList><Latest>b2</Latest><Latest>b3</Latest></BlockList>";
        assert!(put("comp=blocklist", broken).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"hello world");
        // Nothing staged is left beside the file.
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );

        // Through the cache API: a repeated list is a 201, a broken one a 400,
        // and neither changes the upload.
        let cache_dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(cache_dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_create(&h, &token, "k");
        assert_eq!(
            h(url_req(
                "PUT",
                &url,
                "comp=block&blockid=AAAA",
                &[],
                b"data"
            ))
            .status,
            201
        );
        let list = b"<BlockList><Latest>AAAA</Latest></BlockList>";
        for _ in 0..2 {
            assert_eq!(
                h(url_req("PUT", &url, "comp=blocklist", &[], list)).status,
                201
            );
        }
        let broken = b"<BlockList><Latest>AAAA</Latest><Latest>AAAB</Latest></BlockList>";
        assert_eq!(
            h(url_req("PUT", &url, "comp=blocklist", &[], broken)).status,
            400
        );
        let staging = v2.store.lock().unwrap().staging(1);
        assert_eq!(std::fs::read(staging).unwrap(), b"data");
        assert_eq!(v2_finalize(&h, &token, "k", json!("4"))["ok"], true);
        assert_eq!(v2_restore(&h, &token, "k", &[]).unwrap().1, b"data");
    }

    #[test]
    fn cache_v2_finalize_checks_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_create(&h, &token, "k");
        assert_eq!(h(url_req("PUT", &url, "", &[], b"short")).status, 201);
        assert_eq!(
            v2_finalize(&h, &token, "k", json!("6")),
            json!({"ok": false, "message": "uploaded 5 bytes, expected 6"})
        );
        assert_eq!(v2_restore(&h, &token, "k", &[]), None);
        assert!(!v2.store.lock().unwrap().entries[0].complete);
        assert_eq!(
            v2_finalize(&h, &token, "other", json!("5")),
            json!({"ok": false, "message": "no pending upload for this key and version"})
        );
    }

    /// `size_bytes` that is not an unsigned integer, or an upload whose data is
    /// gone, is refused with a reason instead of committing an entry no
    /// download can serve; a missing `size_bytes` still means 0.
    #[test]
    fn cache_v2_finalize_refuses_bad_sizes_and_missing_data() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        v2_create(&h, &token, "k");
        let invalid = json!({"ok": false, "message": "size_bytes is not a valid size"});
        for size in [
            json!("-1"),
            json!(-1),
            json!("twelve"),
            json!(1.5),
            json!(true),
        ] {
            assert_eq!(
                v2_finalize(&h, &token, "k", size.clone()),
                invalid,
                "{size}"
            );
        }
        assert!(!v2.store.lock().unwrap().entries[0].complete);
        let staging = v2.store.lock().unwrap().staging(1);
        std::fs::remove_file(&staging).unwrap();
        assert_eq!(
            v2_finalize(&h, &token, "k", json!("0")),
            json!({"ok": false, "message": "the uploaded data is missing"})
        );
        assert!(!v2.store.lock().unwrap().entries[0].complete);
        assert_eq!(v2_restore(&h, &token, "k", &[]), None);
        // An empty upload with `size_bytes` left out (0) is a valid entry.
        v2_create(&h, &token, "empty");
        let (_, done) = v2_twirp(
            &h,
            Some(&token),
            "FinalizeCacheEntryUpload",
            json!({"key": "empty", "version": "v"}),
        );
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(v2_restore(&h, &token, "empty", &[]).unwrap().1, b"");
    }

    /// A PUT that passed its pending check before a finalize or `gc` and lands
    /// after it neither changes the committed blob nor recreates a dropped
    /// upload: its data is staged, and only put in place while still pending.
    #[test]
    fn cache_v2_late_uploads_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({"scp": "a"}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let paths = |id| {
            let s = v2.store.lock().unwrap();
            (s.blob(id), s.blocks(id), s.staging(id))
        };
        // A retried Put Blob is received while the first one commits.
        let url = v2_create(&h, &token, "k");
        assert_eq!(h(url_req("PUT", &url, "", &[], b"good")).status, 201);
        let (blob, blocks, staging) = paths(1);
        let late =
            stage_block_blob(&url_req("PUT", &url, "", &[], b"evil!"), &blocks, &staging).unwrap();
        assert_eq!(v2_finalize(&h, &token, "k", json!("4"))["ok"], true);
        assert_eq!(v2.place(1, late).status, 404);
        assert_eq!(std::fs::read(&blob).unwrap(), b"good");
        assert!(!staging.exists());
        // A PUT sent after the commit is refused before it is read.
        assert_eq!(h(url_req("PUT", &url, "", &[], b"evil!")).status, 404);
        assert_eq!(v2_restore(&h, &token, "k", &[]).unwrap().1, b"good");
        assert_eq!(blob_files(&v2), ["1"]);

        // A Put Blob and a Put Block are received while `gc` drops the upload.
        let url = v2_create(&h, &token, "gone");
        let (blob, blocks, staging) = paths(2);
        let late_blob =
            stage_block_blob(&url_req("PUT", &url, "", &[], b"late"), &blocks, &staging).unwrap();
        let block = url_req("PUT", &url, "comp=block&blockid=AAAA", &[], b"late");
        let late_block = stage_block_blob(&block, &blocks, &staging).unwrap();
        age(&v2, 2, 10);
        v2.store.lock().unwrap().gc(now());
        assert!(v2.store.lock().unwrap().entries.iter().all(|e| e.id != 2));
        assert_eq!(v2.place(2, late_blob).status, 404);
        assert_eq!(v2.place(2, late_block).status, 404);
        assert!(!blob.exists() && !staging.exists() && !blocks.exists());
        assert_eq!(blob_files(&v2), ["1"]);
    }

    /// While a reservation of a key and version is in use, another job gets
    /// the conflict a complete entry gives ("another job may be creating this
    /// cache"), and a finalize only commits the caller's own reservation. v1
    /// keeps replacing a pending reservation.
    #[test]
    fn cache_v2_one_upload_per_key_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let (a, b) = (
            auth.issue(&json!({"scp": "a"})),
            auth.issue(&json!({"scp": "b"})),
        );
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let conflict = (
            409,
            json!({"code": "already_exists", "msg": "a cache entry with this key and version exists or is being uploaded"}),
        );
        let create = |token: &str, key: &str| {
            v2_twirp(
                &h,
                Some(token),
                "CreateCacheEntry",
                json!({"key": key, "version": "v"}),
            )
        };
        let no_pending =
            json!({"ok": false, "message": "no pending upload for this key and version"});

        let url_a = v2_create(&h, &a, "k");
        assert_eq!(create(&b, "k"), conflict);
        assert_eq!(h(url_req("PUT", &url_a, "", &[], b"AAAA")).status, 201);
        // B's finalize does not commit A's upload.
        assert_eq!(v2_finalize(&h, &b, "k", json!("4")), no_pending);
        assert_eq!(
            v2_finalize(&h, &a, "k", json!("4")),
            json!({"ok": true, "entry_id": "1"})
        );
        assert_eq!(create(&b, "k"), conflict);
        assert_eq!(v2_restore(&h, &b, "k", &[]).unwrap().1, b"AAAA");

        // An abandoned reservation (5 minutes without use) is replaced; its
        // job's late upload and finalize then fail instead of committing B's.
        let url_a = v2_create(&h, &a, "j");
        assert_eq!(h(url_req("PUT", &url_a, "", &[], b"AAAA")).status, 201);
        age(&v2, 2, 6);
        let url_b = v2_create(&h, &b, "j");
        assert!(url_b.contains("/upload/3?"), "{url_b}");
        assert_eq!(h(url_req("PUT", &url_a, "", &[], b"AAAA")).status, 404);
        assert_eq!(h(url_req("PUT", &url_b, "", &[], b"BBBB")).status, 201);
        assert_eq!(v2_finalize(&h, &a, "j", json!("4")), no_pending);
        assert_eq!(
            v2_finalize(&h, &b, "j", json!("4")),
            json!({"ok": true, "entry_id": "3"})
        );
        assert_eq!(v2_restore(&h, &a, "j", &[]).unwrap().1, b"BBBB");

        // v1 is unchanged: a second reservation replaces a pending one.
        let v1_reserve = || {
            h(Request {
                method: "POST".into(),
                path: "/tok/_apis/artifactcache/caches".into(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: json!({"key": "v1", "version": "v"})
                    .to_string()
                    .into_bytes(),
            })
            .status
        };
        assert_eq!((v1_reserve(), v1_reserve()), (201, 201));
        let pending: Vec<u64> = v2
            .store
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.key == "v1")
            .map(|e| e.id)
            .collect();
        assert_eq!(pending, [5]);
    }

    /// Uploads count as use: `gc` keeps a reservation whose blocks still
    /// arrive after 5 minutes, and drops one that got nothing for as long.
    #[test]
    fn cache_v2_uploads_keep_their_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_create(&h, &token, "busy");
        v2_create(&h, &token, "idle");
        age(&v2, 1, 6);
        age(&v2, 2, 6);
        assert_eq!(
            h(url_req(
                "PUT",
                &url,
                "comp=block&blockid=AAAA",
                &[],
                b"data"
            ))
            .status,
            201
        );
        v2.store.lock().unwrap().gc(now());
        let ids: Vec<u64> = v2
            .store
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, [1]);
        age(&v2, 1, 6);
        let list = b"<BlockList><Latest>AAAA</Latest></BlockList>";
        assert_eq!(
            h(url_req("PUT", &url, "comp=blocklist", &[], list)).status,
            201
        );
        v2.store.lock().unwrap().gc(now());
        assert_eq!(v2_finalize(&h, &token, "busy", json!("4"))["ok"], true);
        assert_eq!(v2_restore(&h, &token, "busy", &[]).unwrap().1, b"data");
    }

    /// A reservation that is replaced or dropped takes its blob, staging file
    /// and blocks with it; a new one is in `index.json` at once, so a restart
    /// still collects it.
    #[test]
    fn cache_v2_dropped_reservations_take_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let files = |id| {
            let s = v2.store.lock().unwrap();
            [s.blob(id), s.staging(id), s.blocks(id)].map(|p| p.exists())
        };
        // Replaced by a new reservation of the same key and version.
        let url = v2_create(&h, &token, "k");
        assert_eq!(h(url_req("PUT", &url, "", &[], b"whole")).status, 201);
        assert_eq!(
            h(url_req("PUT", &url, "comp=block&blockid=AAAA", &[], b"x")).status,
            201
        );
        assert_eq!(files(1), [true, true, true]);
        age(&v2, 1, 20);
        let url = v2_create(&h, &token, "k");
        assert_eq!(files(1), [false, false, false]);
        // Recorded at once, and collected by the next start once abandoned.
        let reopened = CacheStore::open(dir.path()).unwrap();
        let pending: Vec<(u64, bool)> = reopened
            .entries
            .iter()
            .map(|e| (e.id, e.complete))
            .collect();
        assert_eq!(pending, [(2, false)]);
        assert!(url.contains("/upload/2?"), "{url}");
        assert_eq!(
            h(url_req("PUT", &url, "comp=block&blockid=AAAA", &[], b"x")).status,
            201
        );
        age(&v2, 2, 20);
        v2.store.lock().unwrap().save();
        let reopened = CacheStore::open(dir.path()).unwrap();
        assert!(reopened.entries.is_empty());
        assert_eq!(files(2), [false, false, false]);
        assert!(blob_files(&v2).is_empty());
    }

    /// With `--cache-server-external-url`, jobs reach the cache port at that
    /// origin (scheme included), so the signed blob URLs name it; the artifact
    /// port, which jobs reach at its `Host`, keeps that.
    #[test]
    fn cache_v2_signs_urls_for_the_external_origin() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = CacheV2 {
            external: Some(url_origin("https://proxy.example/prefix/")),
            ..v2_service(dir.path(), auth.clone())
        };
        let h = |req| cache_server(&v2, "tok", req);
        let url = v2_save(&h, &token, "k", b"data");
        assert!(
            url.starts_with("https://proxy.example/cache-v2/upload/1?"),
            "{url}"
        );
        let (_, found) = v2_twirp(
            &h,
            Some(&token),
            "GetCacheEntryDownloadURL",
            json!({"key": "k", "version": "v"}),
        );
        let download = found["signed_download_url"].as_str().unwrap();
        assert!(
            download.starts_with("https://proxy.example/cache-v2/download/1?"),
            "{download}"
        );
        assert_eq!(v2_restore(&h, &token, "k", &[]).unwrap().1, b"data");

        let artifact_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(ArtifactStore {
            dir: artifact_dir.path().to_path_buf(),
            artifacts: vec![],
            signer: UrlSigner::new(),
            auth,
        }));
        let with = |req| artifact_handler(store.clone(), Some(&v2), req);
        let url = v2_create(&with, &token, "other");
        assert!(url.starts_with("http://h:1/cache-v2/upload/2?"), "{url}");
    }

    /// A body that is not a JSON object gets a JSON Twirp error (not retried)
    /// and changes nothing.
    #[test]
    fn cache_v2_malformed_bodies_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let malformed = (
            400,
            json!({"code": "malformed", "msg": "the request body is not a JSON object"}),
        );
        for method in [
            "CreateCacheEntry",
            "FinalizeCacheEntryUpload",
            "GetCacheEntryDownloadURL",
        ] {
            for body in [br#"{"key":"#.as_slice(), b"", b"[]", br#""k""#, b"null"] {
                assert_eq!(
                    v2_twirp_raw(&h, Some(&token), method, body),
                    malformed,
                    "{method}"
                );
            }
        }
        assert!(v2.store.lock().unwrap().entries.is_empty());
        assert!(blob_files(&v2).is_empty() && !dir.path().join("index.json").exists());
    }

    /// No message repeats the key: the toolkit turns any `msg` containing
    /// `insufficient usage` into a quota error.
    #[test]
    fn cache_v2_messages_do_not_repeat_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let key = "insufficient usage";
        v2_save(&h, &token, key, b"x");
        let (status, again) = v2_twirp(
            &h,
            Some(&token),
            "CreateCacheEntry",
            json!({"key": key, "version": "v"}),
        );
        assert_eq!(status, 409);
        let missing = v2_finalize(&h, &token, "insufficient usage 2", json!("1"));
        assert_eq!(missing["ok"], false);
        for body in [again, missing] {
            assert!(!body.to_string().contains(key), "{body}");
        }
    }

    #[test]
    fn cache_entries_are_shared_between_v1_and_v2() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let v1 =
            |method: &str, rest: &str, query: &[(&str, &str)], range: Option<&str>, body: &[u8]| {
                let mut headers = HashMap::from([("host".to_string(), "h:1".to_string())]);
                if let Some(r) = range {
                    headers.insert("content-range".into(), r.into());
                }
                h(Request {
                    method: method.into(),
                    path: format!("/tok/_apis/artifactcache/{rest}"),
                    query: query
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    headers,
                    body: body.to_vec(),
                })
            };
        // Saved through v1, restored through v2 with an upper-case restore key.
        let reserve = json!({"key": "Shared-Key", "version": "v"}).to_string();
        let created = v1("POST", "caches", &[], None, reserve.as_bytes());
        assert_eq!(created.status, 201);
        let id = serde_json::from_slice::<Value>(&created.body).unwrap()["cacheId"]
            .as_u64()
            .unwrap();
        assert_eq!(
            v1(
                "PATCH",
                &format!("caches/{id}"),
                &[],
                Some("bytes 0-4/*"),
                b"hello"
            )
            .status,
            204
        );
        assert_eq!(
            v1("POST", &format!("caches/{id}"), &[], None, b"").status,
            204
        );
        assert_eq!(
            v2_restore(&h, &token, "nope", &["SHARED-"]),
            Some(("shared-key".to_string(), b"hello".to_vec()))
        );
        // Saved through v2, restored through v1.
        v2_save(&h, &token, "From-V2", b"world");
        let found = v1(
            "GET",
            "cache",
            &[("keys", "from-v2"), ("version", "v")],
            None,
            b"",
        );
        assert_eq!(found.status, 200);
        let found: Value = serde_json::from_slice(&found.body).unwrap();
        assert_eq!(found["cacheKey"], "from-v2");
        let location = found["archiveLocation"].as_str().unwrap();
        let rest = location
            .strip_prefix("http://h:1/tok/_apis/artifactcache/")
            .unwrap();
        assert_eq!(on_wire(&v1("GET", rest, &[], None, b"")).1, b"world");
        // The v2 entry is an ordinary record of `index.json`.
        let reopened = CacheStore::open(dir.path()).unwrap();
        let e = reopened.lookup(&["from-v2".to_string()], "v").unwrap();
        assert_eq!(std::fs::read(reopened.blob(e.id)).unwrap(), b"world");
        // A block upload abandoned for over 5 minutes goes, blocks and all.
        let url = v2_create(&h, &token, "abandoned");
        assert_eq!(
            h(url_req("PUT", &url, "comp=block&blockid=AAAA", &[], b"x")).status,
            201
        );
        let id = v2
            .store
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|e| e.key == "abandoned")
            .unwrap()
            .id;
        assert!(v2.store.lock().unwrap().blocks(id).is_dir());
        age(&v2, id, 10);
        let mut store = v2.store.lock().unwrap();
        store.gc(now());
        assert!(store.entries.iter().all(|e| e.id != id));
        assert!(
            !store.blocks(id).exists() && !store.blob(id).exists() && !store.staging(id).exists()
        );
    }

    #[test]
    fn cache_mode_limits_v2_restore_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = v2_service(dir.path(), RuntimeAuth::new(None));
        let servers = Servers {
            cache_port: Some(1),
            cache_token: "tok".into(),
            cache_external: None,
            artifact_port: None,
            run_id: "7".into(),
            auth: v2.auth.clone(),
        };
        // The token `job_env` issues for a job of that mode.
        let token = |mode| {
            servers
                .job_env("h", "j", mode)
                .into_iter()
                .find(|(k, _)| k == "ACTIONS_RUNTIME_TOKEN")
                .unwrap()
                .1
        };
        let h = |req| cache_server(&v2, "tok", req);
        v2_save(&h, &token(CacheMode::Write), "seed", b"data");
        for (mode, restore, save) in [
            (CacheMode::Write, true, true),
            (CacheMode::Read, true, false),
            (CacheMode::WriteOnly, false, true),
            (CacheMode::Disabled, false, false),
        ] {
            let t = token(mode);
            let name = mode.name();
            // A denied restore is exactly `{"ok": false}` with status 200 (checked by `v2_restore`).
            assert_eq!(v2_restore(&h, &t, "seed", &[]).is_some(), restore, "{name}");
            let key = format!("new-{name}");
            let (status, created) = v2_twirp(
                &h,
                Some(&t),
                "CreateCacheEntry",
                json!({"key": key, "version": "v"}),
            );
            assert_eq!(status, 200);
            // Size 0 is left out, as the toolkit's encoder does.
            let (status, done) = v2_twirp(
                &h,
                Some(&t),
                "FinalizeCacheEntryUpload",
                json!({"key": key, "version": "v"}),
            );
            assert_eq!(status, 200);
            if save {
                assert_eq!(fields(&created), ["ok", "signed_upload_url"], "{name}");
                assert_eq!(created["ok"], true, "{name}");
                assert_eq!(done["ok"], true, "{name}");
            } else {
                let refused = json!({"ok": false, "message": format!("cache-mode {name} does not allow saving")});
                assert_eq!(created, refused);
                assert_eq!(done, refused);
            }
        }
        // A token without the claim (the caller's) is `write`.
        let plain = v2.auth.issue(&json!({}));
        assert!(v2_restore(&h, &plain, "seed", &[]).is_some());
    }

    #[test]
    fn cache_v2_needs_a_runtime_token_and_signed_urls() {
        let dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(dir.path(), auth);
        let h = |req| cache_server(&v2, "tok", req);
        let other_run = RuntimeAuth::new(None).issue(&json!({}));
        let (head, payload) = token.rsplit_once('.').unwrap().0.split_once('.').unwrap();
        let forged = format!("{head}.{payload}.runthrough");
        for bad in [
            None,
            Some(other_run.as_str()),
            Some("garbage"),
            Some(forged.as_str()),
        ] {
            for method in ["CreateCacheEntry", "GetCacheEntryDownloadURL"] {
                let (status, body) = v2_twirp(&h, bad, method, json!({"key": "k", "version": "v"}));
                assert_eq!(status, 401, "{bad:?}");
                assert_eq!(
                    body,
                    json!({"code": "unauthenticated", "msg": "missing or invalid ACTIONS_RUNTIME_TOKEN"})
                );
            }
        }
        assert!(v2.store.lock().unwrap().entries.is_empty());

        // The caller's own token works, as `write`.
        let caller_dir = tempfile::tempdir().unwrap();
        let caller = v2_service(caller_dir.path(), RuntimeAuth::new(Some("ghs_real".into())));
        let hc = |req| cache_server(&caller, "tok", req);
        assert_eq!(
            v2_twirp(&hc, Some(&token), "CreateCacheEntry", json!({"key": "k"})).0,
            401
        );
        v2_save(&hc, "ghs_real", "k", b"x");
        assert!(v2_restore(&hc, "ghs_real", "k", &[]).is_some());

        // Signed URLs: only this server's signature, for that kind and entry, in time.
        let url = v2_create(&h, &token, "k");
        let (base, query) = url.split_once('?').unwrap();
        let tampered = |query: &str| {
            let sig = query.split_once("&sig=").unwrap().1;
            let flipped = format!(
                "{}{}",
                &sig[..sig.len() - 1],
                if sig.ends_with('0') { '1' } else { '0' }
            );
            query.replace(sig, &flipped)
        };
        let past = now() - 1;
        let expired = |kind: &str| {
            let sig = hex::encode(hmac_sha256(
                &v2.signer.key,
                format!("{kind}/1/{past}").as_bytes(),
            ));
            format!("expires={past}&sig={sig}")
        };
        for query in [
            tampered(query),
            expired("cache-upload"),
            v2.signer.sign("upload", 1),
            v2.signer.sign("cache-download", 1),
            UrlSigner::new().sign("cache-upload", 1),
            String::new(),
        ] {
            let put = h(url_req("PUT", &format!("{base}?{query}"), "", &[], b"evil"));
            assert_eq!(put.status, 401, "{query}");
        }
        // The URL itself still works, and only for its own entry.
        assert_eq!(h(url_req("PUT", &url, "", &[], b"good")).status, 201);
        let other = url.replace("/upload/1?", "/upload/2?");
        assert_eq!(h(url_req("PUT", &other, "", &[], b"evil")).status, 401);
        assert_eq!(v2_finalize(&h, &token, "k", json!("4"))["ok"], true);

        // Downloads of the complete entry: the same checks.
        let (_, found) = v2_twirp(
            &h,
            Some(&token),
            "GetCacheEntryDownloadURL",
            json!({"key": "k", "version": "v"}),
        );
        let download = found["signed_download_url"].as_str().unwrap();
        let (base, query) = download.split_once('?').unwrap();
        for query in [
            tampered(query),
            expired("cache-download"),
            v2.signer.sign("cache-upload", 1),
            // An artifact server's URL, with its own key and kind.
            UrlSigner::new().sign("download", 1),
            UrlSigner::new().sign("cache-download", 1),
            String::new(),
        ] {
            let get = h(url_req("GET", &format!("{base}?{query}"), "", &[], b""));
            assert_eq!(get.status, 401, "{query}");
        }
        let get = h(url_req("GET", download, "", &[], b""));
        assert_eq!((get.status, on_wire(&get).1), (200, b"good".to_vec()));
    }

    #[test]
    fn job_env_selects_v2_with_the_cache_server() {
        let servers = |cache: bool, artifact: bool, external: Option<&str>, caller: bool| Servers {
            cache_port: cache.then_some(10),
            cache_token: "tok".into(),
            cache_external: external.map(String::from),
            artifact_port: artifact.then_some(20),
            run_id: "7".into(),
            auth: RuntimeAuth::new(caller.then(|| "ghs_real".to_string())),
        };
        let env = |s: &Servers, mode| {
            let env = s.job_env("h", "j", mode);
            let results = env
                .iter()
                .filter(|(k, _)| k == "ACTIONS_RESULTS_URL")
                .count();
            assert!(results <= 1, "{env:?}");
            env.into_iter().collect::<HashMap<String, String>>()
        };
        let get = |e: &HashMap<String, String>, k: &str| e.get(k).cloned();
        let s = |v: &str| Some(v.to_string());
        // As today, the caller's ACTIONS_RUNTIME_URL wins over the artifact port.
        let runtime_url = std::env::var("ACTIONS_RUNTIME_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "http://h:20/".to_string());

        let e = env(&servers(true, false, None, false), CacheMode::Write);
        assert_eq!(get(&e, "ACTIONS_CACHE_URL"), s("http://h:10/tok/"));
        assert_eq!(get(&e, "ACTIONS_CACHE_SERVICE_V2"), s("true"));
        assert_eq!(get(&e, "ACTIONS_RESULTS_URL"), s("http://h:10/"));
        assert_eq!(get(&e, "ACTIONS_RUNTIME_URL"), None);
        assert!(e.contains_key("ACTIONS_RUNTIME_TOKEN"));

        let e = env(&servers(true, true, None, false), CacheMode::Write);
        assert_eq!(get(&e, "ACTIONS_CACHE_URL"), s("http://h:10/tok/"));
        assert_eq!(get(&e, "ACTIONS_CACHE_SERVICE_V2"), s("true"));
        assert_eq!(get(&e, "ACTIONS_RESULTS_URL"), s("http://h:20/"));
        assert_eq!(get(&e, "ACTIONS_RUNTIME_URL"), Some(runtime_url.clone()));
        assert!(e.contains_key("ACTIONS_RUNTIME_TOKEN"));

        let e = env(&servers(false, true, None, false), CacheMode::Write);
        assert_eq!(get(&e, "ACTIONS_CACHE_URL"), None);
        assert_eq!(get(&e, "ACTIONS_CACHE_SERVICE_V2"), None);
        assert_eq!(get(&e, "ACTIONS_RESULTS_URL"), s("http://h:20/"));
        assert_eq!(get(&e, "ACTIONS_RUNTIME_URL"), Some(runtime_url));
        assert!(e.contains_key("ACTIONS_RUNTIME_TOKEN"));

        assert!(env(&servers(false, false, None, false), CacheMode::Write).is_empty());

        // `Servers::start` keeps the external URL without its trailing slash.
        let e = env(
            &servers(true, false, Some("https://proxy.example/prefix"), false),
            CacheMode::Read,
        );
        assert_eq!(
            get(&e, "ACTIONS_CACHE_URL"),
            s("https://proxy.example/prefix/tok/read/")
        );
        assert_eq!(get(&e, "ACTIONS_CACHE_SERVICE_V2"), s("true"));
        assert_eq!(get(&e, "ACTIONS_RESULTS_URL"), s("https://proxy.example/"));
        assert_eq!(get(&e, "ACTIONS_RUNTIME_URL"), None);
        assert_eq!(
            url_origin("https://proxy.example/prefix/"),
            "https://proxy.example"
        );
        assert_eq!(url_origin("http://10.0.0.1:8080?x"), "http://10.0.0.1:8080");
        // The mode travels in the token the run signs.
        let token = get(&e, "ACTIONS_RUNTIME_TOKEN").unwrap();
        let claims: Value =
            serde_json::from_slice(&base64url_decode(token.split('.').nth(1).unwrap()).unwrap())
                .unwrap();
        assert_eq!(claims["runthrough_cache_mode"], "read");

        // With the caller's token, only `write` jobs get v2; the others get an
        // empty value, which overrides one inherited from runthrough's own
        // environment (the toolkit tests it for truthiness).
        let caller = servers(true, false, None, true);
        for mode in [CacheMode::Read, CacheMode::WriteOnly, CacheMode::Disabled] {
            let e = env(&caller, mode);
            assert_eq!(get(&e, "ACTIONS_RUNTIME_TOKEN"), s("ghs_real"));
            assert_eq!(
                get(&e, "ACTIONS_CACHE_SERVICE_V2"),
                s(""),
                "{}",
                mode.name()
            );
            assert_eq!(
                get(&e, "ACTIONS_CACHE_URL"),
                Some(format!("http://h:10/tok/{}/", mode.name()))
            );
            assert_eq!(get(&e, "ACTIONS_RESULTS_URL"), s("http://h:10/"));
        }
        let e = env(&caller, CacheMode::Write);
        assert_eq!(get(&e, "ACTIONS_CACHE_SERVICE_V2"), s("true"));
    }

    #[test]
    fn artifact_port_forwards_cache_v2() {
        let cache_dir = tempfile::tempdir().unwrap();
        let artifact_dir = tempfile::tempdir().unwrap();
        let auth = RuntimeAuth::new(None);
        let token = auth.issue(&json!({}));
        let v2 = v2_service(cache_dir.path(), auth.clone());
        let store = Arc::new(Mutex::new(ArtifactStore {
            dir: artifact_dir.path().to_path_buf(),
            artifacts: vec![],
            signer: UrlSigner::new(),
            auth,
        }));
        let with = |req| artifact_handler(store.clone(), Some(&v2), req);
        v2_save(&with, &token, "k", b"through the artifact port");
        assert_eq!(
            v2_restore(&with, &token, "k", &[]).unwrap().1,
            b"through the artifact port"
        );
        // Its own API is still there.
        let (status, _) = {
            let r = with(Request {
                method: "POST".into(),
                path: format!("{ARTIFACT_SERVICE}ListArtifacts"),
                query: HashMap::new(),
                headers: HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]),
                body: json!({"workflow_run_backend_id": "r"})
                    .to_string()
                    .into_bytes(),
            });
            (r.status, r.body)
        };
        assert_eq!(status, 200);
        // Without the cache server, a CacheService call gets what it got before.
        let without = |req| artifact_handler(store.clone(), None, req);
        let r = without(Request {
            method: "POST".into(),
            path: format!("{CACHE_SERVICE}CreateCacheEntry"),
            query: HashMap::new(),
            headers: HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]),
            body: json!({"key": "k2", "version": "v"})
                .to_string()
                .into_bytes(),
        });
        assert_eq!((r.status, r.body.is_empty()), (404, true));
        // The cache port points artifact calls at `--artifact-server-path`.
        let h = |req| cache_server(&v2, "tok", req);
        let r = h(Request {
            method: "POST".into(),
            path: format!("{ARTIFACT_SERVICE}CreateArtifact"),
            query: HashMap::new(),
            headers: HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]),
            body: b"{}".to_vec(),
        });
        assert_eq!(r.status, 404);
        assert_eq!(
            serde_json::from_slice::<Value>(&r.body).unwrap(),
            json!({"code": "bad_route", "msg": "the artifact server is off: run with --artifact-server-path"})
        );
    }
}
