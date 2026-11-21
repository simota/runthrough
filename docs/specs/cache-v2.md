# Spec: cache service v2 API

Status: design, not built. Implementers build from this file, one unit per worktree (see
[Units](#units)); the reviewer checks each diff against it.

Goal: `actions/cache@v4` (and anything else bundling `@actions/cache` ≥ 4) saves and restores
through the local cache server when it picks the cache service v2 API, with the same store,
`cache-mode` limits and access control the v1 API and the artifact server have today.

## 1. Protocol facts from the toolkit

Source: the copy of `actions/cache@v4` in the action cache,
`~/.cache/runthrough/actions/actions__cache@v4` (tag `v4.3.0`, commit `0057852b`; its
`package-lock.json` bundles `@actions/cache` `4.1.0`). Every quote below is from
`dist/save/index.js` unless another file is named; the search string finds it. `dist/restore/index.js`
bundles the same library.

### 1.1 How v2 is selected

- `function getCacheServiceVersion`:
  ```js
  if (isGhes()) return 'v1';
  return process.env['ACTIONS_CACHE_SERVICE_V2'] ? 'v2' : 'v1';
  ```
  Any non-empty value selects v2.
- `function isGhes` (in `config.js`): v1 is forced unless `GITHUB_SERVER_URL`'s host is
  `github.com`, ends with `.ghe.com` or ends with `.localhost`. A run with `--github-instance`
  set to a GHES host therefore stays on v1 whatever we set.
- `function getCacheServiceURL`: v2 uses `process.env['ACTIONS_RESULTS_URL'] || ''`; v1 uses
  `ACTIONS_CACHE_URL || ACTIONS_RESULTS_URL`.
- `function isFeatureAvailable`: for v2 `return !!process.env['ACTIONS_RESULTS_URL']`; for v1
  `!!process.env['ACTIONS_CACHE_URL']`. When it is false, the action's own
  `isCacheFeatureAvailable` logs a `[warning]` line and the step continues without a cache.

### 1.2 Twirp client

- `class CacheServiceClient` → `request(service, method, contentType, data)`:
  ```js
  const url = new URL(`/twirp/${service}/${method}`, this.baseUrl).href;
  ```
  The path is absolute: **any path and query in `ACTIONS_RESULTS_URL` are dropped**. The v2 API
  must be served at the root of the origin `ACTIONS_RESULTS_URL` names. The artifact client does
  the same (`actions__upload-artifact@v4/dist/upload/index.js`, search
  ``new URL(`/twirp``), so cache and artifact Twirp calls share one origin.
- Service name `github.actions.results.api.v1.CacheService` (search
  `this.rpc.request("github.actions.results.api.v1.CacheService"`); methods `CreateCacheEntry`,
  `FinalizeCacheEntryUpload`, `GetCacheEntryDownloadURL`. Requests are `POST` with
  `Content-Type: application/json` and the body `JSON.stringify(data)`.
- Auth: `new auth_1.BearerCredentialHandler(token)` with `token = getRuntimeToken()`, which throws
  `Unable to get the ACTIONS_RUNTIME_TOKEN env variable` when it is unset. Every Twirp call carries
  `Authorization: Bearer <ACTIONS_RUNTIME_TOKEN>`.
- Request encoding (`class CacheServiceClientJSON`): `toJson(request, { useProtoFieldName: true,
  emitDefaultValues: false })`, so field names are snake_case proto names and empty fields are
  left out. The generator comment says `long_type_string`: `int64` fields are JSON strings.
- Response decoding: `fromJson(data, { ignoreUnknownFields: true })`. `class ReflectionJsonReader`
  maps `field.name`, `field.jsonName` and `field.localName`, so a response may use snake_case or
  lowerCamelCase names; `int64` may be a string or a number.
- Retries (`retryableRequest`): the body is always parsed with `JSON.parse(rawBody)`. A
  `SyntaxError` (non-JSON body, also on an error status) is treated as **retryable**; so are
  502, 504, 500, 503 and 429 (`isRetryableHttpStatusCode`). Up to 5 attempts, starting at 3 s and
  growing by 1.5×. Any other non-2xx status with a JSON body throws
  `Received non-retryable error: Failed request: (<status>) <reason>: <body.msg>` at once.
  `body.msg` containing `insufficient usage` becomes a `UsageError` (quota) — never send that.
  **Every v2 Twirp response, errors included, must have a JSON body**, or the client retries for
  ~20 s before giving up.

### 1.3 Messages (`results/api/v1/cache.proto`, search `class CreateCacheEntryRequest$Type`)

| Message | Fields (proto name, type) |
|---|---|
| `CreateCacheEntryRequest` | `metadata` (CacheMetadata), `key` string, `version` string |
| `CreateCacheEntryResponse` | `ok` bool, `signed_upload_url` string, `message` string |
| `FinalizeCacheEntryUploadRequest` | `metadata`, `key` string, `size_bytes` int64, `version` string |
| `FinalizeCacheEntryUploadResponse` | `ok` bool, `entry_id` int64, `message` string |
| `GetCacheEntryDownloadURLRequest` | `metadata`, `key` string, `restore_keys` repeated string, `version` string |
| `GetCacheEntryDownloadURLResponse` | `ok` bool, `signed_download_url` string, `matched_key` string |

`CacheMetadata` is `repository_id` int64 and `scope` (repeated `CacheScope`: `scope` string,
`permission` int64). `saveCacheV2` and `restoreCacheV2` never set `metadata`; the server ignores it.

`version` is `getCacheVersion(paths, compressionMethod, enableCrossOsArchive)`: a SHA-256 over the
paths, the compression method, `windows-only` on Windows and a salt (search
`function getCacheVersion`). v1 computes it with the same function (search
`function getCacheEntry` → `utils.getCacheVersion(paths, ...)`), so **an entry is
interchangeable between v1 and v2 when key and version match**.

### 1.4 Save (`function saveCacheV2`)

1. Forces `uploadChunkSize: 64 * 1024 * 1024, uploadConcurrency: 8, useAzureSdk: true`.
2. `CreateCacheEntry({key, version})`. Then:
   ```js
   if (!response.ok) {
       if (response.message) { core.warning(`Cache reservation failed: ${response.message}`); }
       throw new Error(response.message || 'Response was not ok');
   }
   ```
   Any throw here (also a non-retryable HTTP error) is caught and rethrown as
   `ReserveCacheError("Unable to reserve cache with key ${key}, another job may be creating this cache.")`,
   which the outer handler logs with `core.info(`Failed to save: ...`)`. **A refused reservation
   does not fail the step**; with a `message` it also logs a warning first.
3. `cacheHttpClient.saveCache(cacheId, archivePath, signedUploadUrl, options)` →
   `useAzureSdk` → `uploadCacheArchiveSDK(signedUploadURL, archivePath, options)`:
   `new BlobClient(signedUploadURL).getBlockBlobClient().uploadFile(archivePath,
   { blockSize: 64 MiB, concurrency: 8, maxSingleShotSize: 128 * 1024 * 1024 })`.
   In the SDK (`uploadSeekableInternal`): `if (size <= options.maxSingleShotSize) return await
   this.upload(...)` — one **Put Blob** (`PUT <signed url>`, header `x-ms-blob-type: BlockBlob`,
   whole archive as the body). Above 128 MiB: **Put Block** per 64 MiB block
   (`PUT <signed url>&comp=block&blockid=<base64 id>`, up to 8 in parallel), then **Put Block
   List** (`PUT <signed url>&comp=blocklist`, XML `<BlockList><Latest>id</Latest>…</BlockList>`).
   The SDK's operation specs (`uploadOperationSpec`, `stageBlockOperationSpec`,
   `commitBlockListOperationSpec`) expect status **201**; `response._response.status >= 400`
   throws, which the outer handler logs as a warning (`Failed to save: …`), not a step failure.
   The SDK uses the signed URL as given and appends its query parameters to the existing query;
   the artifact server's `/upload/<id>?expires=…&sig=…` URLs are built on the same behaviour
   (`upload-artifact@v4` uses this SDK, search `new storage_blob_1.BlobClient` in its bundle).
4. `FinalizeCacheEntryUpload({key, version, sizeBytes: `${archiveFileSize}`})`. `!ok` with a
   `message` throws `FinalizeCacheError` → `core.warning(message)`; without one, a generic error →
   warning. Success: `cacheId = parseInt(finalizeResponse.entryId)`.

### 1.5 Restore (`function restoreCacheV2`)

1. `GetCacheEntryDownloadURL({key: primaryKey, restoreKeys, version})` (at most 10 keys in all,
   validated client-side).
2. `if (!response.ok) { core.debug(`Cache not found ...`); return undefined; }` — **a miss is
   `ok: false` with status 200**, logged only at debug level.
3. `matched_key` decides the log line (`Cache hit for: …` vs `Cache hit for restore-key: …`). The
   action then compares it with `isExactKeyMatch` (`dist/restore/index.js`, search
   `function isExactKeyMatch`: `localeCompare(key, undefined, { sensitivity: "accent" })`), which
   ignores case, so the store's lowercased keys still count as an exact hit.
4. `lookupOnly` returns after the lookup without downloading.
5. `downloadCache(response.signedDownloadUrl, …)`:
   ```js
   if (archiveUrl.hostname.endsWith('.blob.core.windows.net')) { ...Azure SDK... }
   else { yield (0, downloadUtils_1.downloadCacheHttpClient)(archiveLocation, archivePath); }
   ```
   Our URL is not on `blob.core.windows.net`, so the download is **one plain `GET`** through
   `@actions/http-client`, no `HEAD`, no `Range`. `downloadCacheHttpClient` checks
   `content-length` against the bytes written (`Incomplete download. Expected file size: …`), so the
   response must carry an exact `Content-Length`. `retryHttpClientResponse` retries only 502, 503
   and 504 (`function isRetryableStatusCode`).
6. Any error other than a `ValidationError` is logged (`Failed to restore: …`, warning, or error
   for 5xx) and restore returns `undefined`: **restore never fails the step** except through the
   action's own `fail-on-cache-miss`.

## 2. Design

All server code stays in `src/servers.rs`; `runner.rs` keeps calling
`cfg.servers.job_env(&host_addr, &base_name, cache_mode)` with the same signature.

### 2.1 Where v2 is served

The v2 API is a third route family of the **cache server's** listener, beside the existing
`/<cache_token>/…` v1 routes:

| Route | Auth | Handler |
|---|---|---|
| `POST /twirp/github.actions.results.api.v1.CacheService/<Method>` | runtime token | `cache_v2_twirp` |
| `PUT /cache-v2/upload/<id>?expires=…&sig=…[&comp=block&blockid=…\|&comp=blocklist]` | signature | `cache_v2_blob` |
| `GET /cache-v2/download/<id>?expires=…&sig=…` | signature | `cache_v2_blob` |
| `POST /twirp/github.actions.results.api.v1.ArtifactService/<Method>` | — | `twirp_error(404, "bad_route", "the artifact server is off: run with --artifact-server-path")` |

`/twirp/` and `/cache-v2/` cannot collide with `/<cache_token>/` (32 hex characters).

Because the toolkit drops the path of `ACTIONS_RESULTS_URL` (§1.2) and uses that one variable
for both the cache and the artifact service, the URL a job gets is:

- **cache server only**: `ACTIONS_RESULTS_URL=http://<host>:<cache_port>/`;
- **cache and artifact servers**: `ACTIONS_RESULTS_URL` keeps pointing at the artifact port
  (unchanged), and `artifact_handler` forwards the two cache v2 route families to the cache v2
  handler before its own token check (which the cache handler does itself);
- **artifact server only** (`--no-cache-server --artifact-server-path`): exactly as today.

Rejected alternative: pointing `ACTIONS_RESULTS_URL` at the cache port and forwarding
`ArtifactService` there. It changes the URL `--artifact-server-path` jobs see, which criterion 3
forbids.

Shared state, built in `Servers::start` only when the cache server bound its port:

```rust
#[derive(Clone)]
struct CacheV2 {
    store: Arc<Mutex<CacheStore>>, // the same Arc the v1 handler uses
    signer: UrlSigner,             // own per-run key, see 2.5
    auth: RuntimeAuth,             // clone of Servers.auth
}
```

`cache_handler` gets an extra `&CacheV2` (or the closure captures it) and routes `/twirp/…` and
`/cache-v2/…` to it before the `/<token>/` prefix check. `artifact_handler` gets
`Option<CacheV2>`: `Some` when the cache server runs, `None` with `--no-cache-server` or a failed
cache start; with `None` the routes fall through to today's code. `Servers::start` therefore
keeps the `Arc<Mutex<CacheStore>>` it opens and builds `CacheV2` before starting the artifact
server. `job_env` needs no new field: `cache_port.is_some()` already says the cache server runs.

### 2.2 Twirp handler (`cache_v2_twirp`)

Request fields are read by proto name with the lowerCamelCase form as fallback, as `twirp` does
for artifacts (`field("restoreKeys", "restore_keys")`); `size_bytes` through `json_u64`.
Responses use lowerCamelCase names (proto3 JSON's canonical form, as the artifact `twirp`
responses do); the client accepts both (§1.2). The `host` in signed URLs is the request's `Host`
header, as for artifacts and the v1 `archiveLocation`.

- **`CreateCacheEntry`**
  1. Save denied by the job's cache-mode (§2.4) →
     `200 {"ok": false, "message": "cache-mode <mode> does not allow saving"}`.
  2. `key` lowercased; a complete entry with the same key and version exists →
     `twirp_error(409, "already_exists", "cache entry <key> already exists")`
     (non-retryable → `ReserveCacheError` → info line, as v1's 409).
  3. Otherwise reserve exactly as v1 `POST caches` does (new `next_id`, drop pending entries of
     the same key+version, push `complete: false`, create the empty blob file) — extract that
     body into `CacheStore::reserve(&mut self, key, version) -> Option<u64>` (`None` when a
     complete entry exists) and call it from both APIs.
  4. `200 {"ok": true, "signedUploadUrl": "http://<host>/cache-v2/upload/<id>?<signer.sign("cache-upload", id)>"}`.
- **`FinalizeCacheEntryUpload`**
  1. Save denied → `200 {"ok": false, "message": "cache-mode <mode> does not allow saving"}`.
  2. Find the pending entry with lowercased `key` and `version` → none:
     `200 {"ok": false, "message": "no pending upload for <key>"}`.
  3. The blob file's length differs from `size_bytes` →
     `200 {"ok": false, "message": "uploaded <n> bytes, expected <size_bytes>"}`; the entry stays
     pending and `gc` drops it after 5 minutes.
  4. Mark complete, `used = now()`, `save()`, `gc(now())` (same as v1 commit; extract
     `CacheStore::commit(&mut self, id)` and use it from both), →
     `200 {"ok": true, "entryId": "<id>"}`.
- **`GetCacheEntryDownloadURL`**
  1. Restore denied → `200 {"ok": false}` (a miss).
  2. `keys = [key] ++ restore_keys`; `store.lookup(&keys, &version)` (unchanged: lowercases,
     exact then newest prefix per key, version and `complete` must match). Miss →
     `200 {"ok": false}`.
  3. Hit: set `used = now()`, `save()` (as v1), →
     `200 {"ok": true, "signedDownloadUrl": "http://<host>/cache-v2/download/<id>?<signer.sign("cache-download", id)>", "matchedKey": "<stored key>"}`.
- Unknown method → `twirp_error(404, "bad_route", method)`.

Lock discipline as today: hold the `CacheStore` lock only for index changes, never while
copying blob data.

### 2.3 Blob handler (`cache_v2_blob`)

- `PUT /cache-v2/upload/<id>`: `signer.verify("cache-upload", id, &query)` else `401`; the entry
  must exist and be pending (`!complete`) else `404`; then drop the lock and write:
  - no `comp` (Put Blob): body → `store.blob(id)`;
  - `comp=block`: body → `<cache dir>/blocks/<id>/<hex(blockid)>`;
  - `comp=blocklist`: join the listed blocks in order into `store.blob(id)`, remove
    `blocks/<id>`.
  All three answer `201` with an empty body. This is the logic of today's artifact `blob()`
  `PUT` arm; extract it into one function, e.g.
  `fn write_block_blob(req: &Request, blocks: &Path, file: &Path) -> Result<Option<u64>>`, used
  by both the artifact and the cache handler, so the two cannot drift.
- `GET /cache-v2/download/<id>`: `signer.verify("cache-download", id, &query)` else `401`; the
  entry must exist and be complete else `404`; `Response::file(&store.blob(id),
  "application/octet-stream", None)`, which streams with an exact `Content-Length`
  (`write_response`). No `HEAD` and no `Range` (§1.5 point 5).
- `CacheStore::gc` also removes `blocks/<id>` for every dropped entry (abandoned block uploads).

### 2.4 cache-mode

v1 carries the mode in `ACTIONS_CACHE_URL`'s path. v2 cannot (the path is dropped, §1.2), so the
mode travels in the **runtime token**, which `job_env` already issues per job:

- `job_env` adds a claim `"runthrough_cache_mode": cache_mode.name()` to the claims it signs
  (beside `scp`, `ac`, `nbf`, `exp`). Two jobs with the same name but different modes thus get
  different tokens.
- New `RuntimeAuth::claims(&self, req: &Request) -> Option<Value>`: `None` when `accepts` would
  refuse; for a token this run signed, the decoded JWT payload; for the caller's token
  (`from_env`), `Value::Null` (we cannot read or trust its claims). This needs a small std-only
  base64url decoder in `servers.rs` (the crate has only `actions::base64`, an encoder; no new
  dependency).
- The v2 handler's mode: `claims["runthrough_cache_mode"]` through `CacheMode::parse`, default
  `CacheMode::Write` when the claim is absent.
- **Caller's token in effect** (`ACTIONS_RUNTIME_TOKEN` set in runthrough's own environment): every
  job gets the same token, so the server cannot tell the job's mode. `job_env` then sets
  `ACTIONS_CACHE_SERVICE_V2` only for jobs whose mode is `write`; jobs with `read`, `write-only`
  or `none` keep the v1 API, which still enforces the mode by URL. (See risk R2.)
- Effects, per mode (`CacheMode::can_restore` / `can_save`):

  | mode | `GetCacheEntryDownloadURL` | `CreateCacheEntry` / `Finalize…` |
  |---|---|---|
  | `write` | lookup | allowed |
  | `read` | lookup | `ok: false` + message |
  | `write-only` | `ok: false` (miss) | allowed |
  | `none` | `ok: false` (miss) | `ok: false` + message |

  A denied save surfaces in the step log as `Cache reservation failed: cache-mode read does not
  allow saving` (warning) plus `Failed to save: …` (info), and the step succeeds (§1.4 point 2).
  Blob URLs are only handed out by calls the mode allows, so the blob handler does not check the
  mode (criterion 5: blob URLs by signature only).

### 2.5 Access control

- Twirp: `CacheV2.auth.accepts(&req)` (via `claims`) or
  `twirp_error(401, "unauthenticated", "missing or invalid ACTIONS_RUNTIME_TOKEN")` — the artifact
  API's exact response, JSON body so the client does not retry (§1.2).
- Blob URLs: HMAC-SHA256 over `"<kind>/<id>/<expires>"` with a per-run key, `expires` =
  now + `SIGNED_URL_TTL_MS` (60 min), compared with `same`. This is `ArtifactStore::sign` /
  `verify`; move them into a `#[derive(Clone)] struct UrlSigner { key: Vec<u8> }` with
  `sign(kind, id) -> String` and `verify(kind, id, query) -> bool`, used by `ArtifactStore` (field
  `signer` instead of `key`) and `CacheV2`. The cache gets its **own** key and the kinds
  `cache-upload` / `cache-download`, so an artifact URL can never open a cache blob and vice versa.
- The v1 API keeps its per-run path prefix and no bearer check (unchanged).

### 2.6 Job environment (`Servers::job_env`)

```text
cache  artifact  ACTIONS_CACHE_URL  ACTIONS_CACHE_SERVICE_V2  ACTIONS_RESULTS_URL      ACTIONS_RUNTIME_URL
on     off       v1 URL (as today)  "true" (†)                cache origin + "/"      —
on     on        v1 URL (as today)  "true" (†)                artifact (as today)     as today
off    on        —                  —                         artifact (as today)     as today
off    off       nothing is set (as today)
```

(†) except for non-`write` jobs while the caller's token is in effect (§2.4).

- "cache origin" is `http://<host>:<cache_port>` or, with `--cache-server-external-url`, the
  scheme and authority of that URL (its path is dropped: the toolkit would drop it anyway).
- `ACTIONS_RESULTS_URL` appears at most once in the returned list.
- `ACTIONS_RUNTIME_TOKEN` is set whenever either server runs (as today), now with the
  `runthrough_cache_mode` claim.
- `--no-cache-server`: runthrough sets neither `ACTIONS_CACHE_SERVICE_V2` nor a cache-derived
  `ACTIONS_RESULTS_URL`.

### 2.7 What stays unchanged

v1 routes and responses; `CacheStore` lookup, lowercasing, gc rules and `index.json` format (v2
entries are ordinary `CacheEntry` records; no new field); artifact routes, URLs and responses;
`Servers::job_env`'s signature and its caller in `runner.rs`; `Cargo.toml`.

## 3. Acceptance criteria → approach

| # | Approach | Unit |
|---|---|---|
| 1 Protocol | §2.1–2.3: Twirp routes and signed blob routes on the cache listener (and forwarded from the artifact listener); test drives Create → Put Blob → Finalize → GetURL → GET, and a block-list variant | A |
| 2 Shared store | v2 uses the same `Arc<Mutex<CacheStore>>`, `lookup`, `reserve`/`commit` extracted from v1, `gc`, `save`; tests cross v1↔v2 and reopen the store from `index.json` | A |
| 3 Job environment | §2.6 table in `job_env`; unit tests on `Servers` values for the four server combinations, the external URL and `--no-cache-server` | A |
| 4 cache-mode | §2.4 token claim → `CacheMode`; test per mode | A |
| 5 Access control | §2.5 `RuntimeAuth::claims`, `UrlSigner` with cache kinds; tests for no token, foreign token, caller's token, tampered and expired signatures, artifact-signed URL on the cache route | A |
| 6 No regressions | existing tests untouched in meaning; `make check CARGO_FLAGS=--offline` in every unit; `Cargo.toml` not edited | A, B, C |
| 7 Docs | README, docs/index.html, docs/act-parity.md, CHANGELOG.md, CLAUDE.md as listed in unit C | C |
| 8 Real action | `#[ignore]`d e2e running `actions/cache@v4` save and `actions/cache/restore@v4` in two jobs with `ACTIONS_CACHE_URL` blanked so only v2 can serve them | B |

## 4. Risks and open questions

Each item names the choice this spec takes; the safer one where there was a choice.

- **R1 Denied save: warning or silent info?** v2 can explain a refusal only through `message`,
  which the toolkit logs as a warning (§1.4). A silent `ok: false` gives the misleading
  "another job may be creating this cache". **Choice:** `ok: false` with a message, so the
  reason is visible; the step still succeeds. A conflict with an existing entry (e.g. matrix
  legs saving one key) uses a 409 Twirp error: a quiet info line, like v1's 409. What GitHub's
  service returns in either case is not established.
- **R2 Caller's token and cache-mode.** With `ACTIONS_RUNTIME_TOKEN` inherited from runthrough's
  environment, all jobs share one token, so the server cannot know a job's mode. **Choice:** those
  jobs stay on v1 unless their mode is `write` (mode enforcement never weakens; such jobs do not
  get v2, a narrow exception to "every job" in criterion 3). Alternative not taken: per-mode
  listener ports.
- **R3 Host jobs inherit runthrough's environment.** No `env_clear` exists in `src/` (checked with
  grep), so a `--host` run started inside GitHub Actions passes the real
  `ACTIONS_CACHE_SERVICE_V2` / `ACTIONS_RESULTS_URL` through when runthrough does not override
  them (e.g. with `--no-cache-server`). Pre-existing for v1 variables too. **Choice:** out of
  scope; criterion 3 is read as "runthrough does not set them".
- **R4 BuildKit `type=gha`.** README says the cache server also serves BuildKit's gha cache.
  Whether BuildKit (or `docker/build-push-action`) switches to v2 when `ACTIONS_CACHE_SERVICE_V2`
  is in the job env, and how its Go blob client uploads and downloads (block sizes, `HEAD`,
  `Range`), is **not established**: no BuildKit source is on this machine and no e2e test covers
  it. **Choice:** serve only what the toolkit uses (verified above), keep v1 intact, and record
  it in unit C's docs as unconfirmed. If BuildKit breaks, the first suspects are `HEAD` and
  `Range` on `/cache-v2/download/`.
- **R5 Memory.** `read_request` buffers whole bodies: a Put Blob is up to 128 MiB, and Put Block
  sends 64 MiB × 8 concurrent requests (≈ 512 MiB at peak). The artifact server has the same
  shape today. **Choice:** keep it (no streaming rewrite in this feature); note in the docs only
  if a test shows a problem.
- **R6 Signed URL lifetime.** 60 minutes from `CreateCacheEntry`, checked per request. A >60 min
  upload of blocks would fail its late blocks (warning, no step failure). **Choice:** keep the
  artifact server's TTL.
- **R7 `--cache-server-external-url` with a path prefix.** v1 uses the full URL; v2 can only use
  its origin (§1.2), so a proxy must forward `/twirp/…` and `/cache-v2/…` at its root. Signed URLs
  use the request's `Host` header, as the v1 `archiveLocation` already does. **Choice:** origin
  only; documented in unit C.
- **R8 Artifact actions without `--artifact-server-path`.** `ACTIONS_RESULTS_URL` is now set
  whenever the cache server runs, so `upload-artifact@v4` no longer stops at "Unable to get the
  ACTIONS_RESULTS_URL env variable" but calls the cache port. **Choice:** the cache port answers
  `ArtifactService` with a JSON `404 bad_route` saying to pass `--artifact-server-path`
  (non-retryable, clear message).
- **R9 GHES instances.** `isGhes()` keeps the toolkit on v1 when `GITHUB_SERVER_URL` is not
  github.com / *.ghe.com / *.localhost (§1.1). **Choice:** nothing to do; v1 keeps working.
  Mention in unit C's docs.
- **R10 Other actions.** The cached `Swatinem/rust-cache@v2`, `setup-node@v4`, `setup-go@v5` and
  `setup-python` bundles contain `ACTIONS_CACHE_SERVICE_V2` (grep), so they switch to v2 too,
  including this repo's own `make ci` dogfood run. Older `@actions/cache` (< 4) ignore the
  variable and keep using `ACTIONS_CACHE_URL`. **Choice:** no special handling; unit A's done
  condition includes `make ci` where it can run.

## Units

Three units. **A** owns all of `src/servers.rs`; **B** and **C** touch disjoint files and may be
built in parallel with A from this spec, but **merge order is A, then B, then C**. B's ignored
test and C's docs describe behaviour that exists only after A is merged; rebase B and C on the
merged A before running their done conditions.

### Unit A — server, environment, auth (criteria 1–6)

- **Files:** `src/servers.rs` only.
- **Delivers:** §2.1–2.6: `CacheV2`, `cache_v2_twirp`, `cache_v2_blob`, routing in
  `cache_handler` and forwarding in `artifact_handler`, `CacheStore::reserve` / `commit` shared
  with v1, `blocks/<id>` cleanup in `gc`, `write_block_blob` shared with the artifact `blob()`,
  `UrlSigner` replacing `ArtifactStore::sign`/`verify`, `RuntimeAuth::claims` with a base64url
  decoder, the `runthrough_cache_mode` claim and the new `job_env` table. Updates the module doc
  comment at the top of `servers.rs` to list the v2 API.
- **Tests** (in `servers.rs`'s `mod tests`, driving the handlers with `Request` values as the
  existing tests do):
  - `cache_v2_save_then_restore_returns_the_bytes` — Create → Put Blob (`x-ms-blob-type:
    BlockBlob`) → Finalize (`size_bytes` as a string) → GetCacheEntryDownloadURL → GET on the
    signed URL returns the same bytes; `matchedKey` is the lowercased key.
  - `cache_v2_block_upload_joins_blocks` — two `comp=block` PUTs + `comp=blocklist` → download
    equals the concatenation; `blocks/<id>` is gone.
  - `cache_v2_finalize_checks_the_size` — mismatched `size_bytes` → `ok: false`, entry not
    restorable.
  - `cache_entries_are_shared_between_v1_and_v2` — save via v1 (reserve/PATCH/commit), restore
    via v2 with an upper-case restore-key prefix; save via v2, restore via v1 `GET cache`; reopen
    `CacheStore::open` on the same dir and find the v2 entry through `lookup`; a pending v2 block
    upload older than 5 minutes is dropped by `gc` with its `blocks/<id>`.
  - `cache_mode_limits_v2_restore_and_save` — for each of `read`, `write`, `write-only`, `none`,
    a token issued with that claim: lookup hit or `ok: false`, Create allowed or
    `ok: false` + message, per the §2.4 table.
  - `cache_v2_needs_a_runtime_token_and_signed_urls` — no `Authorization`, a token signed with
    another key, and a garbage token → 401 JSON `unauthenticated`; a `from_env` token accepted
    (mode `write`); upload and download URLs with a changed `sig`, a past `expires` (validly
    signed) or an artifact-kind signature → 401.
  - `job_env_selects_v2_with_the_cache_server` — `Servers` literals for cache only, cache +
    artifact, artifact only, neither, cache with external URL `https://proxy.example/prefix/`,
    and caller's token with a `read` job; asserts each variable of the §2.6 table and that
    `ACTIONS_RESULTS_URL` occurs at most once.
  - `artifact_port_forwards_cache_v2` — `artifact_handler` with `Some(CacheV2)` serves a v2
    round trip; with `None` a CacheService call gets today's answer.
  - Existing tests (`cache_mode_limits_restore_and_save`, `artifact_blocks_join_and_download`,
    `artifact_api_calls_need_a_runtime_token_of_this_run`, `hmac_matches_rfc4231_and_urls_verify`,
    `runtime_token_grants_buildkit_a_writable_scope`, …) keep passing, adjusted only for the
    `UrlSigner` move.
- **Done when:** `make check CARGO_FLAGS=--offline` passes; `cargo test --offline servers::`
  lists the new tests as passing; `git diff --stat main -- . ':!src/servers.rs'` is empty;
  outside the sandbox, `cargo test --offline --test e2e -- --ignored cache_and_artifact_servers_roundtrip`
  still passes and `make ci CARGO_FLAGS=--offline` succeeds (R10; report it as skipped if Docker
  or ports are unavailable).

### Unit B — real-action e2e (criterion 8)

- **Files:** `tests/e2e.rs`, new `tests/fixtures/repo/.github/workflows/cache-v2.yml`.
- **Delivers:** a fixture workflow on its own event, `registry_package` (unused by the other
  fixtures; checked with grep):
  ```yaml
  name: cache-v2
  on: registry_package
  jobs:
    v2-save:
      runs-on: ubuntu-latest
      env:
        ACTIONS_CACHE_URL: ""   # only the v2 API can serve this job
      steps:
        - run: test -n "$ACTIONS_CACHE_SERVICE_V2" && test -z "$ACTIONS_CACHE_URL"
        - run: mkdir -p v2-cache && echo "v2 roundtrip" > v2-cache/data.txt
        - uses: actions/cache@v4
          with: { path: v2-cache, key: v2-roundtrip }
    v2-restore:
      needs: v2-save
      runs-on: ubuntu-latest
      env:
        ACTIONS_CACHE_URL: ""
      steps:
        - uses: actions/cache/restore@v4
          with: { path: v2-cache, key: v2-roundtrip, fail-on-cache-miss: true }
        - run: echo "cache v2 restored: $(cat v2-cache/data.txt)"
  ```
  Blanking `ACTIONS_CACHE_URL` makes v1 unavailable (§1.1), so a pass proves v2. The first step
  proves the job env really overrides the server's variable (if it does not, put the override
  where it does and say so in the report). Job IDs `v2-save` / `v2-restore` must not clash with
  `list_and_dryrun_print_stages`' lookups.
- **Tests:** `#[ignore = "binds local ports and needs actions/cache@v4 (cached or fetched from github.com)"]`
  `fn real_actions_cache_v4_saves_and_restores_through_v2()`: `bin()` with
  `-C FIXTURE --host registry_package --cache-server-path <tempdir>` (no `--no-cache-server`, the
  default action cache so a cached copy is reused), assert stdout contains
  `cache v2 restored: v2 roundtrip` and `cache-v2 / v2-restore: success`, and that
  `<tempdir>/index.json` holds a complete entry with key `v2-roundtrip`.
- **Done when:** `make check CARGO_FLAGS=--offline` passes (the new workflow must not break the
  non-ignored e2e tests, e.g. `-l` listing or `--validate`); after rebasing on merged A, outside
  the sandbox `cargo test --offline --test e2e -- --ignored real_actions_cache_v4_saves_and_restores_through_v2`
  passes. If it cannot run (no port binding, action not cached and no network), report criterion
  8 as unconfirmed with the reason.

### Unit C — docs (criterion 7)

- **Files:** `README.md`, `docs/index.html`, `docs/act-parity.md`, `CHANGELOG.md`, `CLAUDE.md`.
- **Delivers:**
  - `README.md`: drop "The cache service v2 API (`ACTIONS_CACHE_SERVICE_V2`)." from
    "Not supported"; in the features bullet about the cache server say it serves the v1 API
    (`ACTIONS_CACHE_URL`) and the v2 API (`ACTIONS_CACHE_SERVICE_V2`, through
    `ACTIONS_RESULTS_URL`), that v2 calls need the run's `ACTIONS_RUNTIME_TOKEN`, and that
    `cache-mode` applies to both.
  - `docs/index.html`: remove the "Cache service v2" card from the `#gaps` section; the features
    list item for the cache server mentions both APIs.
  - `docs/act-parity.md`: the cache-server line under "Workflow features" (line with
    `the @actions/cache v1 API (ACTIONS_CACHE_URL)`) adds the v2 API; the `cache-mode` line of
    the seventh pass says v2 is limited the same way (denied restore = miss, denied save =
    `ok: false` with a message, logged as a warning without failing the step); note R4
    (BuildKit over v2 unconfirmed) and R9 (GHES stays on v1).
  - `CHANGELOG.md`: a new `## Unreleased` section above `## 0.3.0` with `### Added`: the cache
    server answers the cache service v2 API (`ACTIONS_CACHE_SERVICE_V2`) used by
    `actions/cache@v4`, sharing entries with v1; jobs get `ACTIONS_CACHE_SERVICE_V2` and, without
    `--artifact-server-path`, an `ACTIONS_RESULTS_URL` pointing at the cache server.
  - `CLAUDE.md`, the `servers.rs` paragraph: add the v2 API (Twirp `CacheService` on the cache
    port or forwarded from the artifact port, signed `/cache-v2/` blob URLs, cache-mode carried
    as the `runthrough_cache_mode` token claim, `ACTIONS_RESULTS_URL` rules).
- **Tests:** none (docs only).
- **Done when:** `grep -n 'ACTIONS_CACHE_SERVICE_V2' README.md docs/index.html` shows no
  "not supported"/"not served" wording; `grep -n 'Unreleased' CHANGELOG.md` finds the section;
  every statement matches merged unit A (check against `src/servers.rs`); `make check
  CARGO_FLAGS=--offline` passes.
