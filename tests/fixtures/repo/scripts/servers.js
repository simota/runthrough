// Exercises the cache (@actions/cache v1) and artifact (@actions/artifact v2)
// protocols the way the toolkit does.
const assert = (c, m) => { if (!c) { console.error("FAIL " + m); process.exit(1); } };
const auth = { Authorization: `Bearer ${process.env.ACTIONS_RUNTIME_TOKEN}` };

async function cache() {
  const base = process.env.ACTIONS_CACHE_URL;
  assert(base && base.endsWith("/"), "ACTIONS_CACHE_URL");
  const api = (r) => `${base}_apis/artifactcache/${r}`;
  const key = `k-${process.pid}-${Date.now()}`;
  let r = await fetch(api(`cache?keys=${key}&version=v1`), { headers: auth });
  assert(r.status === 204, `miss ${r.status}`);
  r = await fetch(api("caches"), { method: "POST", headers: { ...auth, "Content-Type": "application/json" },
    body: JSON.stringify({ key, version: "v1", cacheSize: 6 }) });
  const { cacheId } = await r.json();
  assert(r.status === 201 && cacheId, "reserve");
  for (const [start, chunk] of [[0, "abc"], [3, "def"]]) {
    r = await fetch(api(`caches/${cacheId}`), { method: "PATCH",
      headers: { ...auth, "Content-Type": "application/octet-stream", "Content-Range": `bytes ${start}-${start + 2}/*` }, body: chunk });
    assert(r.status === 204, "upload chunk");
  }
  r = await fetch(api(`caches/${cacheId}`), { method: "POST", headers: { ...auth, "Content-Type": "application/json" },
    body: JSON.stringify({ size: 6 }) });
  assert(r.status === 204, "commit");
  r = await fetch(api(`cache?keys=nothing,${key.slice(0, 2).toUpperCase()}&version=v1`), { headers: auth });
  const entry = await r.json();
  assert(entry.cacheKey === key.toLowerCase(), "case-insensitive prefix restore");
  const outside = new URL(base);
  outside.pathname = "/_apis/artifactcache/cache";
  r = await fetch(`${outside}?keys=${key}&version=v1`);
  assert(r.status === 404, `API outside the per-run prefix answered ${r.status}`);
  const body = await (await fetch(entry.archiveLocation)).text();
  assert(body === "abcdef", `download ${body}`);
  console.log("cache roundtrip ok");
}

async function artifacts() {
  const base = process.env.ACTIONS_RESULTS_URL;
  assert(base, "ACTIONS_RESULTS_URL");
  const scp = JSON.parse(Buffer.from(process.env.ACTIONS_RUNTIME_TOKEN.split(".")[1], "base64url")).scp;
  const [, run, job] = scp.split(" ").find((s) => s.startsWith("Actions.Results:")).split(":");
  const twirp = async (method, body) => {
    const r = await fetch(new URL(`/twirp/github.actions.results.api.v1.ArtifactService/${method}`, base),
      { method: "POST", headers: { ...auth, "Content-Type": "application/json" },
        body: JSON.stringify({ workflowRunBackendId: run, workflowJobRunBackendId: job, ...body }) });
    return r.json();
  };
  const name = `art-${process.pid}`;
  const created = await twirp("CreateArtifact", { name, version: 4 });
  assert(created.ok && created.signedUploadUrl, "create");
  const ids = ["YmxvY2sx", "YmxvY2sy"];
  for (const [id, data] of [[ids[0], "PK-one|"], [ids[1], "two"]]) {
    const r = await fetch(`${created.signedUploadUrl}&comp=block&blockid=${encodeURIComponent(id)}`, { method: "PUT", body: data });
    assert(r.status === 201, "stage block");
  }
  const xml = `<?xml version="1.0" encoding="utf-8"?><BlockList>${ids.map((i) => `<Latest>${i}</Latest>`).join("")}</BlockList>`;
  let r = await fetch(`${created.signedUploadUrl}&comp=blocklist`, { method: "PUT", body: xml });
  assert(r.status === 201, "commit blocks");
  const fin = await twirp("FinalizeArtifact", { name, size: "10" });
  assert(fin.ok && fin.artifactId, "finalize");
  const list = await twirp("ListArtifacts", {});
  assert(list.artifacts.some((a) => a.name === name && a.size === "10"), "list");
  const { signedUrl } = await twirp("GetSignedArtifactURL", { name });
  const body = await (await fetch(signedUrl)).text();
  assert(body === "PK-one|two", `download ${body}`);
  const forged = new URL(signedUrl);
  forged.searchParams.set("sig", "0".repeat(64));
  r = await fetch(forged);
  assert(r.status === 401, `forged signature answered ${r.status}`);
  const del = await twirp("DeleteArtifact", { name });
  assert(del.ok, "delete");
  console.log("artifact roundtrip ok");
}

({ cache, artifacts })[process.argv[2]]().catch((e) => { console.error(e); process.exit(1); });
