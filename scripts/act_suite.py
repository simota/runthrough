"""Run act's runner integration tests (pkg/runner/runner_test.go) through runthrough.

Usage: python3 -I scripts/act_suite.py <act checkout> <runthrough binary> <out dir> [case-filter]

Covers the TestRunEvent table (jobs in containers), the TestRunEventHostEnvironment
tables (jobs on the host; act runs those on Linux only), and the single-workflow tests
(secrets, inputs, pull_request payload, --matrix, masking, post-step and pull failures).
Each case is run with act's test settings and compared with act's expected success or
failure. Prints one line per case and a summary; logs go to <out dir>.
"""
import os
import re
import shutil
import subprocess
import sys

act, binary, out = sys.argv[1], sys.argv[2], sys.argv[3]
only = sys.argv[4] if len(sys.argv) > 4 else None
runner = os.path.join(act, "pkg/runner")
testdata = os.path.join(runner, "testdata")
src = open(os.path.join(runner, "runner_test.go")).read()
os.makedirs(out, exist_ok=True)

CONTAINER = {"ubuntu-latest": "node:24-bookworm-slim", "self-hosted": "-self-hosted"}
HOST = {"ubuntu-latest": "-self-hosted", "self-hosted": "-self-hosted"}
ROW = re.compile(r'\{(workdir|"[^"]+"), "([^"]+)", "([^"]+)", "([^"]*)", (platforms|map\[string\]string\{[^}]*\}), ')


def table(start, end, platforms):
    """Rows of a `[]TestJobFileInfo{...}` literal between two markers."""
    body = src[src.index(start):]
    body = body[:body.index(end)]
    rows = []
    for line in body.splitlines():
        line = line.strip()
        m = ROW.match(line) if line.startswith("{") else None
        if not m:
            continue
        wd, case, event, err, plat = m.groups()
        root = testdata if wd == "workdir" else os.path.normpath(os.path.join(runner, wd.strip('"')))
        p = dict(platforms)
        for label, image in re.findall(r'"([\w.-]+)": "([^"]+)"', plat):
            p[label] = image
        secrets = re.findall(r'"(\w+)": "([^"]*)"', line.split(plat, 1)[1]) if '{"secret"' in line else []
        rows.append(dict(suite=start.split("(")[0].split()[-1], root=root, case=case, event=event,
                         fail=bool(err), platforms=p,
                         args=[x for k, v in secrets for x in ("-s", f"{k}={v}")]))
    return rows


cases = table("func TestRunEvent(", "for _, table := range tables", CONTAINER)
host = src[src.index("func TestRunEventHostEnvironment("):src.index("func TestDryrunEvent(")]
for start in ['if runtime.GOOS == "linux" {', '} else {']:
    seg = host[host.index(start):]
    seg = seg[:seg.index("}...)")]
    for line in seg.splitlines():
        m = ROW.match(line.strip()) if line.strip().startswith("{") else None
        if m:
            wd, case, event, err, _ = m.groups()
            cases.append(dict(suite="TestRunEventHostEnvironment", root=testdata, case=case,
                              event=event, fail=bool(err), platforms=HOST, args=[]))


def single(case, event="push", fail=False, platforms=CONTAINER, args=(), absent=()):
    cases.append(dict(suite="single", root=testdata, case=case, event=event, fail=fail,
                      platforms=platforms, args=list(args), absent=list(absent)))


sec = os.path.join(testdata, "secrets")
single("secrets", args=["--env-file", os.path.join(sec, ".env"), "--secret-file", os.path.join(sec, ".secrets")])
single("input-from-cli", event="workflow_dispatch", args=["--input", "SOME_INPUT=input"])
single("pull-request", event="pull_request")
single("matrix-with-user-inclusions", args=["--matrix", "node:8", "--matrix", "node:8.x", "--matrix", "os:ubuntu-18.04"])
single("mask-values", absent=["composite secret", "YWJjCg=="])
single("post-step-failure-is-job-failure", fail=True, platforms=HOST)
single("checkout", fail=True, platforms={"ubuntu-latest": "localhost:0000/missing:latest"})

# Cases that fail for reasons outside runthrough get another image.
# docker-action-host-env builds FROM debian:bullseye-slim, whose security archive is gone:
# run a copy of the case whose Dockerfile uses bookworm.
patched = os.path.join(out, "patched")
case_dir = os.path.join(patched, "docker-action-host-env")
shutil.rmtree(patched, ignore_errors=True)
shutil.copytree(os.path.join(testdata, "docker-action-host-env"), case_dir)
dockerfile = os.path.join(case_dir, "action/Dockerfile")
text = open(dockerfile).read().replace("debian:bullseye-slim", "debian:bookworm-slim")
open(dockerfile, "w").write(text)
subprocess.run(["git", "init", "-q", patched], check=True)
for c in cases:
    if c["case"] == "docker-action-host-env":
        c["root"] = patched
# act runs the host tables on Linux only; elsewhere the host has no pwsh and its /bin/sh
# may be bash, so run those jobs in Linux images that have them.
if sys.platform != "linux":
    HOST_IMAGES = {"shells/pwsh": "catthehacker/ubuntu:pwsh-latest", "shells/sh": "node:24-bookworm-slim"}
    for c in cases:
        if c["suite"] == "TestRunEventHostEnvironment" and c["case"] in HOST_IMAGES:
            c["platforms"] = {**c["platforms"], "ubuntu-latest": HOST_IMAGES[c["case"]]}

results = []
for c in cases:
    if only and only not in c["case"]:
        continue
    wf = os.path.join(c["root"], c["case"])
    cmd = [binary, "-C", c["root"], "-W", wf, c["event"], "--pull=false",
           # act's test runner: no workflow recursion, act's default actor.
           "--no-recurse", "--actor", "nektos/act",
           "--action-cache-dir", os.path.join(out, "actions"),
           "--cache-server-path", os.path.join(out, "cache")]
    for label, image in c["platforms"].items():
        cmd += ["-P", f"{label}={image}"]
    if os.path.exists(os.path.join(wf, "event.json")):
        cmd += ["-e", os.path.join(wf, "event.json")]
    conf = os.path.join(wf, "config/config.yml")
    if os.path.exists(conf):
        for line in open(conf):
            m = re.match(r"\s+(\S+@\S+):\s*(\S+)", line)
            if m:
                cmd += ["--local-repository", f"{m.group(1)}={os.path.join(runner, m.group(2))}"]
    cmd += c["args"]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
        code, text = p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired as e:
        code, text = -1, (e.stdout or b"").decode() + "\nTIMEOUT"
    name = f"{c['suite']}-{c['case'].replace('/', '_')}-{c['event']}"
    open(os.path.join(out, name + ".log"), "w").write(" ".join(cmd) + "\n\n" + text)
    ok = (code != 0) if c["fail"] else (code == 0)
    leaked = [s for s in c.get("absent", []) if s in text]
    ok = ok and not leaked
    results.append(ok)
    note = f" leaked={leaked}" if leaked else ""
    print(f"{'PASS' if ok else 'FAIL'} {c['suite']} {c['case']} [{c['event']}] "
          f"expect={'failure' if c['fail'] else 'success'} exit={code}{note}", flush=True)

print(f"\n{sum(results)}/{len(results)} cases matched act's expectation")
