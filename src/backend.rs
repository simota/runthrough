//! Execution environments: a container (via the `docker` CLI or Apple's `container`
//! CLI) or the host.
//!
//! The job's scratch directory is bind-mounted at the *same absolute path* inside
//! the container, so host and container paths are interchangeable.

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub enum Backend {
    Host,
    Container(Container),
}

/// Which CLI manages containers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Engine {
    Docker,
    /// Apple's `container` CLI (macOS 26+, Apple silicon).
    Container,
}

impl Engine {
    pub fn program(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::Container => "container",
        }
    }

    pub fn command(self) -> Command {
        Command::new(self.program())
    }

    /// Run a command and return stdout; `Err` carries stderr.
    fn run(self, args: &[&str]) -> Result<String> {
        let out = self.command().args(args).output().with_context(|| {
            format!(
                "running `{}` (is it installed and on PATH?)",
                self.program()
            )
        })?;
        if !out.status.success() {
            bail!(
                "{} {} failed: {}",
                self.program(),
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Fail with a clear message when the engine's daemon is not running (or does
    /// not answer within `PROBE_TIMEOUT`), instead
    /// of on the first pull. Only success is remembered, so a `--watch` rerun
    /// after starting the engine probes again.
    pub fn ensure_running(self) -> Result<()> {
        static RUNNING: AtomicBool = AtomicBool::new(false);
        if RUNNING.load(Ordering::Relaxed) {
            return Ok(());
        }
        let args: &[&str] = match self {
            Engine::Docker => &["version", "--format", "{{.Server.Version}}"],
            Engine::Container => &["system", "status"],
        };
        let mut cmd = self.command();
        cmd.args(args);
        self.check(cmd, PROBE_TIMEOUT)?;
        RUNNING.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// `ensure_running`'s verdict on the probe `cmd`.
    fn check(self, cmd: Command, timeout: Duration) -> Result<()> {
        let (hint, hung_hint) = match self {
            Engine::Docker => (
                "start Docker Desktop (or the Docker daemon), or run the jobs on the host with --host",
                "restart Docker Desktop (or the Docker daemon), or run the jobs on the host with --host",
            ),
            Engine::Container => (
                "start it with `container system start`, or run the jobs on the host with --host",
                "restart it with `container system stop` and `container system start`, or run the jobs on the host with --host",
            ),
        };
        let probe = format!(
            "{} {}",
            self.program(),
            cmd.get_args()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = output_within(cmd, timeout).with_context(|| {
            format!(
                "running `{}` (is it installed and on PATH?)",
                self.program()
            )
        })?;
        let Some(out) = out else {
            bail!(
                "the {} engine did not answer `{probe}` within {}s and may be hung: {hung_hint}",
                self.program(),
                timeout.as_secs_f64()
            );
        };
        if !out.status.success() {
            let mut err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if err.is_empty() {
                err = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            bail!(
                "the {} engine is not running: {hint}\n  `{probe}`: {err}",
                self.program()
            );
        }
        Ok(())
    }

    fn quiet_ok(self, args: &[&str]) -> bool {
        self.command()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Make sure `image` is present, pulling it when missing or when `pull` is set.
    pub fn ensure_image(
        self,
        image: &str,
        pull: bool,
        platform: Option<&str>,
        credentials: Option<&Credentials>,
    ) -> Result<()> {
        self.ensure_running()?;
        let program = self.program();
        let exists = self
            .command()
            .args(["image", "inspect", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running `{program}` (is it installed and on PATH?)"))?
            .success();
        if pull || !exists {
            eprintln!("  🐳 {program} image pull {image}");
            let mut cmd = self.command();
            // Logged in with a throwaway config so the user's own stays untouched.
            let _config = match credentials {
                Some(c) => {
                    let dir = self.login(image, c)?;
                    cmd.arg("--config").arg(dir.path());
                    Some(dir)
                }
                None => None,
            };
            cmd.args(["image", "pull"]);
            cmd.args(self.quiet_args());
            if let Some(p) = platform {
                cmd.args(["--platform", p]);
            }
            let st = cmd.arg(image).stdout(Stdio::null()).status()?;
            if !st.success() && !exists {
                bail!("{program} image pull {image} failed");
            }
        }
        Ok(())
    }

    /// `docker login` into a fresh config directory for pulling `image`.
    fn login(self, image: &str, c: &Credentials) -> Result<tempfile::TempDir> {
        if self != Engine::Docker {
            bail!("registry credentials are supported with --engine docker only");
        }
        let dir = tempfile::Builder::new()
            .prefix("runthrough-docker-")
            .tempdir()?;
        let mut cmd = self.command();
        cmd.arg("--config").arg(dir.path()).args([
            "login",
            "--username",
            &c.username,
            "--password-stdin",
        ]);
        if let Some(registry) = registry_of(image) {
            cmd.arg(registry);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(c.password.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "docker login for {image} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(dir)
    }

    /// Build `dockerfile` in `context` as `tag`, unless it exists and `rebuild` is false.
    pub fn build_image(
        self,
        context: &Path,
        dockerfile: &Path,
        tag: &str,
        rebuild: bool,
    ) -> Result<()> {
        if !rebuild && self.quiet_ok(&["image", "inspect", tag]) {
            return Ok(());
        }
        eprintln!(
            "  🐳 {} build -t {tag} {}",
            self.program(),
            context.display()
        );
        let out = self
            .command()
            .args(["build", "-t", tag, "-f"])
            .arg(dockerfile)
            .arg(context)
            .output()?;
        if !out.status.success() {
            bail!(
                "{} build of {} failed:\n{}",
                self.program(),
                dockerfile.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Whether bind mounts reach host files through a VM's file sharing, as
    /// Docker Desktop does outside Linux; the runner then keeps paths that see
    /// heavy writes on volumes.
    pub fn shares_files_through_vm(self) -> bool {
        self == Engine::Docker && !cfg!(target_os = "linux")
    }

    /// Options that keep `run` and `image pull` from drawing progress output.
    pub fn quiet_args(self) -> &'static [&'static str] {
        match self {
            Engine::Docker => &[],
            Engine::Container => &["--progress", "none"],
        }
    }

    /// The `run` option that lets containers reach the host's servers as
    /// `host.docker.internal` (`host_address`); Docker Desktop on macOS has it built in.
    pub fn host_gateway_arg(self) -> Option<&'static str> {
        (self == Engine::Docker && !cfg!(target_os = "macos"))
            .then_some("--add-host=host.docker.internal:host-gateway")
    }

    /// Whether containers can join the host's network (`--network host`).
    pub fn has_host_network(self) -> bool {
        self == Engine::Docker
    }

    /// Whether containers are reachable by `--network-alias` names; without
    /// them the runner writes service addresses into `/etc/hosts`.
    pub fn has_network_aliases(self) -> bool {
        self == Engine::Docker
    }

    /// Whether `run` takes `--health-*` options; without them `wait_healthy`
    /// polls the health command (see `take_health`).
    pub fn has_health_checks(self) -> bool {
        self == Engine::Docker
    }

    /// Whether bind mounts break GNU tar extracting `./` into them, so job
    /// containers get RUNNER_TEMP on a volume.
    pub fn needs_temp_volume(self) -> bool {
        self == Engine::Docker
    }

    /// Whether containers reach the host through the vmnet gateway, so the
    /// host's servers must listen beyond loopback.
    pub fn reaches_host_through_gateway(self) -> bool {
        self == Engine::Container
    }

    /// A Docker engine on another machine (`engine_is_remote`): job files
    /// live in volumes kept in sync with `docker cp`.
    pub fn is_remote(self) -> bool {
        self == Engine::Docker && engine_is_remote()
    }

    pub fn volume_create(self, name: &str) -> Result<()> {
        self.run(&["volume", "create", name]).map(|_| ())
    }

    pub fn volume_remove(self, name: &str) {
        let _ = self.quiet_ok(&["volume", "rm", "-f", name]);
    }

    /// Create `dir` as root and open it to the image's user (like act's chown).
    pub fn mkdir(self, container: &str, dir: &Path) -> Result<()> {
        let d = dir.to_string_lossy();
        // One round trip: each engine call costs a connection with a remote engine.
        self.run(&[
            "exec",
            "-u",
            "0",
            container,
            "sh",
            "-c",
            "mkdir -p \"$1\" && chmod a+rwx \"$1\"",
            "sh",
            &d,
        ])
        .map(|_| ())
    }

    /// Copy host file `file` to the same path in `container`, writable by any user.
    pub fn copy_file_in(self, container: &str, file: &Path) -> Result<()> {
        let f = file.to_string_lossy();
        self.run(&["cp", &f, &format!("{container}:{f}")])?;
        self.run(&["exec", "-u", "0", container, "chmod", "a+rw", &f])
            .map(|_| ())
    }

    /// Copy the contents of host directory `dir` to the same path in `container`.
    pub fn copy_in(self, container: &str, dir: &Path) -> Result<()> {
        let d = dir.to_string_lossy();
        self.run(&["exec", "-u", "0", container, "mkdir", "-p", &d])?;
        self.run(&["cp", &format!("{d}/."), &format!("{container}:{d}")])?;
        // `docker cp` writes as root; images that run as another user must
        // still be able to write their outputs and workspace.
        self.run(&["exec", "-u", "0", container, "chmod", "-R", "a+rwX", &d])
            .map(|_| ())
    }

    /// Copy the contents of `dir` in `container` back to the same host path.
    pub fn copy_out(self, container: &str, dir: &Path) -> Result<()> {
        let d = dir.to_string_lossy();
        std::fs::create_dir_all(dir)?;
        self.run(&["cp", &format!("{container}:{d}/."), &d])
            .map(|_| ())
    }

    pub fn network_create(self, name: &str) -> Result<()> {
        self.run(&["network", "create", name]).map(|_| ())
    }

    pub fn network_remove(self, name: &str) {
        let _ = self.quiet_ok(&["network", "rm", name]);
    }

    pub fn remove(self, name: &str) {
        let _ = self.quiet_ok(&["rm", "-f", name]);
    }

    fn inspect_json(self, name: &str) -> Option<serde_json::Value> {
        self.run(&["inspect", name])
            .ok()
            .and_then(|out| serde_json::from_str(&out).ok())
    }

    /// `container port -> host port` mappings of a running container.
    pub fn ports(self, name: &str) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        match self {
            Engine::Docker => {
                if let Ok(text) = self.run(&["port", name]) {
                    // "5432/tcp -> 0.0.0.0:55000"
                    for line in text.lines() {
                        if let Some((c, h)) = line.split_once(" -> ") {
                            let c = c.split('/').next().unwrap_or(c).trim().to_string();
                            let h = h.rsplit(':').next().unwrap_or(h).trim().to_string();
                            out.entry(c).or_insert(h);
                        }
                    }
                }
            }
            Engine::Container => {
                let published = self
                    .inspect_json(name)
                    .and_then(|v| v.pointer("/0/configuration/publishedPorts").cloned());
                for p in published
                    .as_ref()
                    .and_then(|p| p.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let (Some(c), Some(h)) =
                        (p["containerPort"].as_u64(), p["hostPort"].as_u64())
                    {
                        out.entry(c.to_string()).or_insert(h.to_string());
                    }
                }
            }
        }
        out
    }

    /// Running containers that publish host port `port` (`5432/tcp`). Apple
    /// containers are not looked up: their engine's own error stands.
    pub fn port_holders(self, port: &str) -> Vec<String> {
        match self {
            Engine::Docker => self
                .run(&[
                    "ps",
                    "--filter",
                    &format!("publish={port}"),
                    "--format",
                    "{{.Names}}",
                ])
                .map(|out| out.lines().map(str::to_string).collect())
                .unwrap_or_default(),
            Engine::Container => vec![],
        }
    }

    /// IPv4 address of an Apple container on its first network.
    pub fn ipv4(self, name: &str) -> Option<String> {
        self.inspect_json(name)
            .and_then(|v| find_key(&v, "ipv4Address"))
            .map(|a| a.split('/').next().unwrap_or(&a).to_string())
    }

    /// Address a container started by this engine uses to reach the host.
    pub fn host_address(self, name: &str) -> String {
        match self {
            Engine::Docker => "host.docker.internal".into(),
            // Apple containers reach the host through their network's gateway.
            Engine::Container => self
                .inspect_json(name)
                .and_then(|v| find_key(&v, "ipv4Gateway"))
                .map(|g| g.split('/').next().unwrap_or(&g).to_string())
                .unwrap_or_else(|| "192.168.64.1".into()),
        }
    }

    /// Wait until a service container is healthy: Docker reports it; for Apple
    /// containers, which have no health checks, `health` is polled with `exec`.
    pub fn wait_healthy(self, name: &str, health: Option<&Health>) -> Result<()> {
        let start = Instant::now();
        match (self, health) {
            (Engine::Docker, _) => loop {
                let status = self
                    .run(&[
                        "inspect",
                        "--format",
                        "{{if .State.Health}}{{.State.Health.Status}}{{end}}",
                        name,
                    ])
                    .unwrap_or_default();
                match status.trim() {
                    "" | "healthy" => return Ok(()),
                    "unhealthy" => bail!("service container {name} is unhealthy"),
                    _ if start.elapsed() > Duration::from_secs(600) => {
                        bail!("service container {name} did not become healthy")
                    }
                    _ => std::thread::sleep(Duration::from_secs(1)),
                }
            },
            (Engine::Container, Some(h)) => {
                for _ in 0..h.retries.max(1) {
                    std::thread::sleep(h.interval);
                    if self.quiet_ok(&["exec", name, "sh", "-c", &h.cmd]) {
                        return Ok(());
                    }
                }
                bail!(
                    "service container {name} is unhealthy (`{}` kept failing)",
                    h.cmd
                )
            }
            (Engine::Container, None) => Ok(()),
        }
    }
}

/// A Docker-style health check, emulated for engines without one.
#[derive(Debug, PartialEq)]
pub struct Health {
    pub cmd: String,
    pub interval: Duration,
    pub retries: u32,
}

/// Split `--health-*` options out of a service's options.
pub fn take_health(args: Vec<String>) -> (Vec<String>, Option<Health>) {
    let mut rest = Vec::new();
    let (mut cmd, mut interval, mut retries) = (None, Duration::from_secs(10), 3u32);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--health") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        if !flag.starts_with("--health") && flag != "--no-healthcheck" {
            rest.push(a);
            continue;
        }
        if flag == "--no-healthcheck" {
            continue;
        }
        let value = inline.or_else(|| it.next()).unwrap_or_default();
        match flag.as_str() {
            "--health-cmd" => cmd = Some(value),
            "--health-interval" => interval = parse_duration(&value).unwrap_or(interval),
            "--health-retries" => retries = value.parse().unwrap_or(retries),
            _ => {} // --health-timeout, --health-start-period: not emulated
        }
    }
    (
        rest,
        cmd.map(|cmd| Health {
            cmd,
            interval,
            retries,
        }),
    )
}

/// `10s`, `1m30s`, `500ms`, `2` (seconds).
fn parse_duration(s: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        let unit = match (c, chars.peek()) {
            ('m', Some('s')) => {
                chars.next();
                0.001
            }
            ('h', _) => 3600.0,
            ('m', _) => 60.0,
            ('s', _) => 1.0,
            _ => return None,
        };
        total += Duration::from_secs_f64(n * unit);
    }
    if !num.is_empty() {
        total += Duration::from_secs_f64(num.parse().ok()?);
    }
    Some(total)
}

/// Split an options string like a shell would (quotes, no expansion).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Registry login for a private image (`container.credentials`).
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Registry host of an image reference; `None` for Docker Hub.
fn registry_of(image: &str) -> Option<&str> {
    let (first, rest) = image.split_once('/')?;
    (!rest.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost"))
        .then_some(first)
}

#[derive(Clone)]
pub struct Container {
    pub engine: Engine,
    pub name: String,
    pub image: String,
    /// Left running after the job (`--reuse`).
    pub keep: bool,
}

pub struct ContainerSpec<'a> {
    pub engine: Engine,
    pub image: &'a str,
    pub name: String,
    /// Host paths mounted at the same path in the container.
    pub mounts: Vec<PathBuf>,
    pub env: IndexMap<String, String>,
    /// The job's `container.options` / a service's `options`, already split.
    pub options: Vec<String>,
    /// Options from the command line (`--container-options`, `--privileged`, ...).
    pub extra_args: Vec<String>,
    pub extra_volumes: Vec<String>,
    pub ports: Vec<String>,
    pub network: Option<String>,
    pub aliases: Vec<String>,
    pub pull: bool,
    pub platform: Option<&'a str>,
    pub credentials: Option<Credentials>,
    /// Keep the container alive with `tail -f /dev/null` instead of the image's entrypoint.
    pub keepalive: bool,
    /// A service's `entrypoint` and `command`, overriding the image's.
    pub entrypoint: Option<String>,
    pub command: Vec<String>,
    /// Reuse a container of the same name if one exists, and leave it running afterwards.
    pub reuse: bool,
}

impl Backend {
    pub fn start_container(spec: ContainerSpec) -> Result<Backend> {
        let engine = spec.engine;
        let program = engine.program();
        let container = || {
            Backend::Container(Container {
                engine,
                name: spec.name.clone(),
                image: spec.image.to_string(),
                keep: spec.reuse,
            })
        };
        if spec.reuse && engine.quiet_ok(&["inspect", &spec.name]) {
            let _ = engine.quiet_ok(&["start", &spec.name]);
            eprintln!("  ♻️  reusing container {}", spec.name);
            return Ok(container());
        }
        engine.ensure_image(
            spec.image,
            spec.pull,
            spec.platform,
            spec.credentials.as_ref(),
        )?;
        let mut cmd = engine.command();
        cmd.args(["run", "-d", "--name", &spec.name]);
        cmd.args(engine.quiet_args());
        if spec.keepalive {
            cmd.args(["--entrypoint", "tail"]);
        } else if let Some(e) = &spec.entrypoint {
            cmd.args(["--entrypoint", e]);
        }
        if let Some(p) = spec.platform {
            cmd.args(["--platform", p]);
        }
        if let Some(n) = &spec.network {
            cmd.args(["--network", n]);
        }
        // Without aliases (Apple containers) the runner writes /etc/hosts instead.
        // Aliases exist only on user-defined networks (as in act).
        let user_network = spec
            .network
            .as_deref()
            .is_some_and(|n| !matches!(n, "host" | "bridge" | "none" | "default"));
        if engine.has_network_aliases() && user_network {
            for a in &spec.aliases {
                cmd.args(["--network-alias", a]);
            }
        }
        for p in &spec.ports {
            cmd.args(["-p", p]);
        }
        for m in &spec.mounts {
            let m = m.to_string_lossy();
            cmd.args(["-v", &format!("{m}:{m}")]);
        }
        for v in &spec.extra_volumes {
            cmd.args(["-v", v]);
        }
        for (k, v) in &spec.env {
            cmd.args(["-e", &format!("{k}={v}")]);
        }
        cmd.args(&spec.extra_args);
        cmd.args(&spec.options);
        cmd.arg(spec.image);
        if spec.keepalive {
            cmd.args(["-f", "/dev/null"]);
        }
        cmd.args(&spec.command);
        let out = cmd.output().context("starting container")?;
        if !out.status.success() {
            bail!(
                "{program} run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(container())
    }

    /// `runner.os` value for this backend.
    pub fn runner_os(&self) -> &'static str {
        match self {
            Backend::Container(_) => "Linux",
            Backend::Host => match std::env::consts::OS {
                "macos" => "macOS",
                "windows" => "Windows",
                _ => "Linux",
            },
        }
    }

    pub fn runner_arch(&self) -> &'static str {
        match std::env::consts::ARCH {
            "aarch64" => "ARM64",
            "arm" => "ARM",
            "x86" => "X86",
            _ => "X64",
        }
    }

    /// The PATH a command gets without an explicit one: the host's, or the
    /// container's (its image's unless the job overrode it).
    pub fn path(&self) -> String {
        const DEFAULT: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
        match self {
            Backend::Host => std::env::var("PATH").unwrap_or_default(),
            Backend::Container(_) => {
                let mut path = String::new();
                let argv = ["printenv".to_string(), "PATH".to_string()];
                let code = self.exec(
                    &argv,
                    &IndexMap::new(),
                    Path::new("/"),
                    None,
                    None,
                    &mut |l| path = l.trim().to_string(),
                );
                match code {
                    Ok(0) if !path.is_empty() => path,
                    _ => DEFAULT.into(),
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Backend::Host => "host".into(),
            Backend::Container(c) => format!("{}:{}", c.engine.program(), c.image),
        }
    }

    /// Run a command, streaming each output line to `on_line`. Returns the exit code.
    /// Setting `stop` terminates the command's process group: SIGTERM, then
    /// SIGKILL after a grace period.
    pub fn exec(
        &self,
        argv: &[String],
        env: &IndexMap<String, String>,
        cwd: &Path,
        timeout: Option<Duration>,
        stop: Option<&AtomicBool>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<i32> {
        static PID_SEQ: AtomicUsize = AtomicUsize::new(0);
        // In a container, `docker exec` starts the command as a process group
        // leader; its PID is written where a later `exec` can find it.
        let pid_file = (stop.is_some() && matches!(self, Backend::Container(_))).then(|| {
            format!(
                "/tmp/runthrough-{}-{}.pid",
                std::process::id(),
                PID_SEQ.fetch_add(1, Ordering::SeqCst)
            )
        });
        let mut cmd = match self {
            #[cfg(unix)]
            Backend::Host if stop.is_some() => {
                use std::os::unix::process::CommandExt;
                // Its own process group keeps the terminal's Ctrl-C from reaching
                // the command, so a watcher on stdin (a pipe whose other end only
                // this process holds) ends the group once runthrough is gone.
                let mut c = Command::new("sh");
                c.args(["-c", HOST_GROUP_WRAPPER, "sh"])
                    .args(argv)
                    .current_dir(cwd)
                    .envs(env)
                    .process_group(0);
                c
            }
            Backend::Host => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]).current_dir(cwd).envs(env);
                c
            }
            Backend::Container(ct) => {
                let mut c = ct.engine.command();
                c.args(["exec", "-w"]).arg(cwd);
                for (k, v) in env {
                    c.arg("-e").arg(format!("{k}={v}"));
                }
                c.arg(&ct.name);
                if let Some(f) = &pid_file {
                    c.args(["sh", "-c", &format!("echo $$ > {f}; exec \"$@\""), "sh"]);
                }
                c.args(argv);
                c
            }
        };
        let watched = cfg!(unix) && stop.is_some() && matches!(self, Backend::Host);
        cmd.stdin(if watched {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", argv[0]))?;
        // `wait` would close stdin first; the watcher must see EOF only after.
        let watch = child.stdin.take();
        let rx = pump(&mut child);
        let start = Instant::now();
        let mut stopping: Option<Instant> = None;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => on_line(&line),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if timeout.is_some_and(|t| start.elapsed() > t) {
                if stop.is_some() {
                    self.signal_group(child.id(), pid_file.as_deref(), "KILL");
                }
                let _ = child.kill();
                self.remove_pid_file(pid_file.as_deref());
                bail!("timed out after {:?}", timeout.unwrap());
            }
            match stopping {
                None if stop.is_some_and(|s| s.load(Ordering::SeqCst)) => {
                    self.signal_group(child.id(), pid_file.as_deref(), "TERM");
                    stopping = Some(Instant::now());
                }
                Some(at) if at.elapsed() > STOP_GRACE => {
                    self.signal_group(child.id(), pid_file.as_deref(), "KILL");
                    let _ = child.kill();
                }
                _ => {}
            }
        }
        let status = child.wait()?;
        drop(watch);
        self.remove_pid_file(pid_file.as_deref());
        Ok(status.code().unwrap_or(-1))
    }

    fn remove_pid_file(&self, pid_file: Option<&str>) {
        if let (Backend::Container(ct), Some(f)) = (self, pid_file) {
            let _ = ct
                .engine
                .command()
                .args(["exec", &ct.name, "rm", "-f", f])
                .output();
        }
    }

    /// Send `signal` to a command's process group: on the host the child's PID
    /// is its group (on Windows, its process tree is ended), in a container the
    /// group's PID is read from `pid_file`.
    fn signal_group(&self, child: u32, pid_file: Option<&str>, signal: &str) {
        let _ = match (self, pid_file) {
            (Backend::Container(ct), Some(f)) => ct
                .engine
                .command()
                .args(["exec", &ct.name, "sh", "-c"])
                // dash's `kill` takes no `--`.
                .arg(format!("kill -{signal} -$(cat {f})"))
                .output(),
            // Windows has no process groups to signal: end the process tree.
            _ if cfg!(windows) => Command::new("taskkill")
                .args(["/T", "/F", "/PID", &child.to_string()])
                .output(),
            _ => Command::new("kill")
                .arg(format!("-{signal}"))
                .arg("--")
                .arg(format!("-{child}"))
                .output(),
        };
    }

    pub fn stop(&self) {
        if let Backend::Container(c) = self {
            if !c.keep {
                c.engine.remove(&c.name);
            }
        }
    }
}

/// Whether the Docker daemon is on another machine (`DOCKER_HOST=ssh://…` or
/// `tcp://…`), where host paths cannot be bind-mounted. `RUNTHROUGH_REMOTE_ENGINE=1`
/// forces the same handling, e.g. for engines in a VM without shared folders.
fn engine_is_remote() -> bool {
    let forced = std::env::var("RUNTHROUGH_REMOTE_ENGINE").is_ok_and(|v| !v.is_empty() && v != "0");
    let remote_host = std::env::var("DOCKER_HOST")
        .is_ok_and(|h| h.starts_with("ssh://") || h.starts_with("tcp://"));
    forced || remote_host
}

/// First string value under `key` anywhere in a JSON document.
fn find_key(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => m
            .get(key)
            .and_then(|x| x.as_str().map(String::from))
            .or_else(|| m.values().find_map(|x| find_key(x, key))),
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

/// How long a stopped command gets between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Runs `"$@"` while a background subshell waits for EOF on stdin. EOF comes
/// when runthrough closes the pipe: after reaping the command (which then no
/// longer exists, so nothing is killed) or by dying, in which case the
/// command's process group is terminated.
const HOST_GROUP_WRAPPER: &str = "p=$$; exec 3<&0; \
(read -r _ <&3; kill -0 $p 2>/dev/null && kill -TERM 0) </dev/null >/dev/null 2>&1 & \
exec \"$@\" </dev/null 3<&-";

/// How long the engine may take to answer `ensure_running`'s probe. A daemon
/// that is up answers at once; a hung one (Docker Desktop whose VM stopped
/// responding) never does, and every job would wait on it without a word.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// `cmd`'s output, or `None` (the command killed) once it runs past `timeout`.
/// Its output is read after it exits, so it must fit a pipe: a short status.
fn output_within(mut cmd: Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().map(Some)
}

/// Lines a step's output may run ahead of their handling: past it the readers
/// wait, and so does the command writing to the pipe.
const PENDING_LINES: usize = 1024;

/// Longest line passed on at once; a longer one (output without newlines, such
/// as a progress bar redrawn with `\r`) arrives in pieces of this size.
const MAX_LINE: u64 = 1 << 20;

/// Forward stdout and stderr lines from a child into one channel.
fn pump(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::sync_channel(PENDING_LINES);
    fn spawn_reader<R: Read + Send + 'static>(r: R, tx: mpsc::SyncSender<String>) {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(r);
            let mut buf = Vec::new();
            while (&mut reader)
                .take(MAX_LINE)
                .read_until(b'\n', &mut buf)
                .map(|n| n > 0)
                .unwrap_or(false)
            {
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                if tx.send(line).is_err() {
                    break;
                }
                buf.clear();
            }
        });
    }
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, tx);
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_hung_engine_probe_fails_instead_of_waiting() {
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let started = Instant::now();
        let e = Engine::Docker
            .check(sh("sleep 30"), Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(5), "{e}");
        assert!(
            e.contains("did not answer `docker -c sleep 30` within 0.3s"),
            "{e}"
        );
        assert!(e.contains("restart Docker Desktop"), "{e}");
        let e = Engine::Docker
            .check(
                sh("echo 'cannot connect' >&2; exit 1"),
                Duration::from_secs(5),
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("engine is not running"), "{e}");
        assert!(e.contains("cannot connect"), "{e}");
        assert!(Engine::Docker
            .check(sh("echo 27.0"), Duration::from_secs(5))
            .is_ok());
    }

    fn piped(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Output that is not handled yet holds its writer back instead of piling up.
    #[cfg(unix)]
    #[test]
    fn unhandled_output_holds_the_command_back() {
        let mut child = piped("yes | head -n 200000");
        let rx = pump(&mut child);
        std::thread::sleep(Duration::from_millis(500));
        assert!(child.try_wait().unwrap().is_none(), "the command ran ahead");
        assert_eq!(rx.iter().count(), 200_000);
        assert!(child.wait().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    fn long_lines_arrive_in_pieces() {
        let mut child = piped("head -c 2500000 /dev/zero | tr '\\0' a; printf '\\nx\\n'");
        let lines: Vec<String> = pump(&mut child).iter().collect();
        child.wait().unwrap();
        let lens: Vec<usize> = lines.iter().map(String::len).collect();
        assert_eq!(lens, [1 << 20, 1 << 20, 2_500_000 - (2 << 20), 1]);
        assert_eq!(lines[3], "x");
    }

    #[test]
    fn registries() {
        assert_eq!(registry_of("ghcr.io/o/i:1"), Some("ghcr.io"));
        assert_eq!(registry_of("localhost:5000/i"), Some("localhost:5000"));
        assert_eq!(registry_of("library/debian"), None);
        assert_eq!(registry_of("debian:12"), None);
    }

    #[test]
    fn splits_quoted_options() {
        assert_eq!(
            split_args(r#"--health-cmd "pg_isready -U x" --health-interval=2s  -e 'A B'"#),
            vec![
                "--health-cmd",
                "pg_isready -U x",
                "--health-interval=2s",
                "-e",
                "A B"
            ]
        );
    }

    #[test]
    fn extracts_health_options() {
        let args = split_args(
            r#"--cpus 2 --health-cmd "pg_isready" --health-interval=1m30s --health-retries 5 --health-timeout 5s"#,
        );
        let (rest, h) = take_health(args);
        assert_eq!(rest, vec!["--cpus", "2"]);
        assert_eq!(
            h,
            Some(Health {
                cmd: "pg_isready".into(),
                interval: Duration::from_secs(90),
                retries: 5
            })
        );
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("2"), Some(Duration::from_secs(2)));
    }
}
