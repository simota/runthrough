//! Resolving `uses:` references and loading `action.yml` metadata.

use crate::workflow::Step;
use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value as Yaml;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum ActionRef {
    /// `./path/to/action` relative to the workspace.
    Local(String),
    /// `owner/repo[/sub/path]@ref`
    Remote {
        owner: String,
        repo: String,
        path: String,
        git_ref: String,
    },
    /// `docker://image`
    Docker(String),
}

impl ActionRef {
    pub fn parse(uses: &str) -> Result<ActionRef> {
        let uses = uses.trim();
        if uses.starts_with("./") || uses.starts_with("../") {
            return Ok(ActionRef::Local(uses.to_string()));
        }
        if let Some(img) = uses.strip_prefix("docker://") {
            return Ok(ActionRef::Docker(img.to_string()));
        }
        let (spec, git_ref) = uses
            .rsplit_once('@')
            .with_context(|| format!("action reference '{uses}' is missing '@ref'"))?;
        let mut parts = spec.splitn(3, '/');
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            bail!("invalid action reference '{uses}'");
        };
        Ok(ActionRef::Remote {
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: parts.next().unwrap_or("").to_string(),
            git_ref: git_ref.to_string(),
        })
    }

    /// Whether this is a `setup-*` action (`actions/setup-python`, `ruby/setup-ruby`,
    /// …): one that installs tools into the runner's tool cache.
    pub fn installs_tools(&self) -> bool {
        matches!(self, ActionRef::Remote { repo, .. } if repo.to_ascii_lowercase().starts_with("setup-"))
    }

    pub fn is_checkout(&self) -> bool {
        matches!(self, ActionRef::Remote { owner, repo, path, .. }
            if owner.eq_ignore_ascii_case("actions") && repo.eq_ignore_ascii_case("checkout") && path.is_empty())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ActionDef {
    #[serde(default)]
    pub inputs: IndexMap<String, ActionInput>,
    #[serde(default)]
    pub outputs: IndexMap<String, ActionOutput>,
    pub runs: Runs,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ActionInput {
    pub default: Option<Yaml>,
    #[serde(default)]
    pub required: Option<Yaml>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ActionOutput {
    pub value: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Runs {
    pub using: String,
    pub main: Option<String>,
    pub pre: Option<String>,
    #[serde(rename = "pre-if")]
    pub pre_if: Option<String>,
    pub post: Option<String>,
    #[serde(rename = "post-if")]
    pub post_if: Option<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// `using: docker`: `Dockerfile` (relative to the action) or `docker://image`.
    pub image: Option<String>,
    pub entrypoint: Option<String>,
    #[serde(rename = "pre-entrypoint")]
    pub pre_entrypoint: Option<String>,
    #[serde(rename = "post-entrypoint")]
    pub post_entrypoint: Option<String>,
    #[serde(default)]
    pub args: Vec<Yaml>,
    #[serde(default)]
    pub env: IndexMap<String, Yaml>,
}

impl ActionDef {
    pub fn load(dir: &Path) -> Result<ActionDef> {
        let Some(file) = ["action.yml", "action.yaml"]
            .iter()
            .map(|f| dir.join(f))
            .find(|p| p.is_file())
        else {
            // Like act, a bare Dockerfile is a Docker action with no inputs.
            if dir.join("Dockerfile").is_file() {
                return Ok(ActionDef {
                    inputs: IndexMap::new(),
                    outputs: IndexMap::new(),
                    runs: Runs {
                        using: "docker".into(),
                        image: Some("Dockerfile".into()),
                        ..Runs::default()
                    },
                });
            }
            bail!("no action.yml or Dockerfile in {}", dir.display());
        };
        let text = std::fs::read_to_string(&file)?;
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", file.display()))
    }

    pub fn is_node(&self) -> bool {
        self.runs.using.starts_with("node")
    }
}

/// One lock per cached action: two jobs never clone the same action at once,
/// and a clone never holds up a job whose actions are already cached.
static FETCH_LOCKS: Mutex<Vec<(PathBuf, Arc<Mutex<()>>)>> = Mutex::new(Vec::new());

fn fetch_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut locks = FETCH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, lock)) = locks.iter().find(|(d, _)| d == dir) {
        return lock.clone();
    }
    let lock = Arc::new(Mutex::new(()));
    locks.push((dir.to_path_buf(), lock.clone()));
    lock
}

/// Where remote actions and repositories come from (`--github-instance` and friends).
#[derive(Clone, Debug)]
pub struct GithubHost {
    pub instance: String,
    /// `owner/repo` (lowercase) fetched from github.com even on a GHE instance.
    pub replace_with_github_com: Vec<String>,
    pub github_com_token: Option<String>,
    /// `GITHUB_TOKEN` for the instance itself, so private actions and
    /// reusable workflows can be cloned (as act does).
    pub token: Option<String>,
}

impl GithubHost {
    /// Clone URL and optional token for `owner/repo`.
    pub fn source(&self, owner: &str, repo: &str) -> (String, Option<String>) {
        let slug = format!("{owner}/{repo}").to_ascii_lowercase();
        if self.instance != "github.com" && self.replace_with_github_com.contains(&slug) {
            return (
                format!("https://github.com/{owner}/{repo}"),
                self.github_com_token.clone(),
            );
        }
        (
            format!("https://{}/{owner}/{repo}", self.instance),
            self.token.clone(),
        )
    }
}

/// `git` with an optional bearer token passed as a header (never stored in `.git/config`).
fn git_cmd(token: Option<&str>) -> Command {
    let mut c = Command::new("git");
    // A transfer stalled below 1 KB/s for a minute fails instead of holding
    // the job (and its slot) forever.
    c.args([
        "-c",
        "advice.detachedHead=false",
        "-c",
        "http.lowSpeedLimit=1000",
        "-c",
        "http.lowSpeedTime=60",
    ]);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        let basic = base64(format!("x-access-token:{t}").as_bytes());
        c.args([
            "-c",
            &format!("http.extraHeader=Authorization: basic {basic}"),
        ]);
    }
    c
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A full SHA-1 or SHA-256 commit ID, which `git clone --branch` does not accept.
fn is_commit_sha(r: &str) -> bool {
    matches!(r.len(), 40 | 64) && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Clone `url` at `git_ref` (a branch, tag or commit SHA; `None` for the default branch) into `dir`.
pub fn clone_at(url: &str, git_ref: Option<&str>, dir: &Path, token: Option<&str>) -> Result<()> {
    if !git_ref.is_some_and(is_commit_sha) {
        let mut clone = git_cmd(token);
        clone.args(["clone", "--quiet", "--depth", "1"]);
        if let Some(r) = git_ref {
            clone.args(["--branch", r]);
        }
        if clone.arg(url).arg(dir).status()?.success() {
            return Ok(());
        }
    }
    let Some(git_ref) = git_ref else {
        bail!("git clone {url} failed");
    };
    // A commit SHA cannot be cloned with --branch; fetch it explicitly.
    let _ = std::fs::remove_dir_all(dir);
    let git = |args: &[&str]| -> Result<()> {
        let st = git_cmd(token).arg("-C").arg(dir).args(args).status()?;
        if !st.success() {
            bail!("git {} failed for {url}", args.join(" "));
        }
        Ok(())
    };
    std::fs::create_dir_all(dir)?;
    git(&["init", "--quiet"])?;
    git(&["remote", "add", "origin", url])?;
    git(&["fetch", "--quiet", "--depth", "1", "origin", git_ref]).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(dir);
    })?;
    git(&["checkout", "--quiet", "FETCH_HEAD"])
}

/// Clone (or reuse a cached clone of) a remote action and return its directory.
pub fn fetch_remote(
    cache: &Path,
    host: &GithubHost,
    owner: &str,
    repo: &str,
    git_ref: &str,
    offline: bool,
) -> Result<PathBuf> {
    let dir = cache.join(format!("{owner}__{repo}@{}", git_ref.replace('/', "-")));
    let lock = fetch_lock(&dir);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if dir.join(".git").is_dir() {
        return Ok(dir);
    }
    if offline {
        bail!("action {owner}/{repo}@{git_ref} is not cached and --offline is set");
    }
    std::fs::create_dir_all(cache)
        .with_context(|| format!("creating action cache {}", cache.display()))?;
    let (url, token) = host.source(owner, repo);
    eprintln!("  ⬇️  fetching {owner}/{repo}@{git_ref}");
    clone_at(&url, Some(git_ref), &dir, token.as_deref())?;
    Ok(dir)
}

/// Content-addressed files of a `.git` directory (`rel` is relative to it):
/// loose objects, packs with their indexes, and Git LFS objects. Git never
/// rewrites one in place (it writes a new file and renames it), so checkouts
/// can share them.
fn immutable(rel: &Path) -> bool {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    match parts.as_slice() {
        ["objects", "pack", name] => name.starts_with("pack-"),
        ["objects", dir, _] => dir.len() == 2 && dir.bytes().all(|b| b.is_ascii_hexdigit()),
        ["lfs", "objects", ..] => true,
        _ => false,
    }
}

/// The repository's shareable `.git` files, copied once per run: each
/// checkout hardlinks them instead of copying them again. Only this copy is
/// linked, never the user's own repository.
#[derive(Default)]
pub struct ObjectSnapshot(Mutex<Option<(PathBuf, tempfile::TempDir)>>);

impl ObjectSnapshot {
    /// The snapshot of `git_dir`, holding `files` (relative to it) when the
    /// first checkout takes it; `None` when it cannot be taken, or was taken
    /// of another repository.
    fn dir(&self, git_dir: &Path, files: &[PathBuf]) -> Option<PathBuf> {
        let mut taken = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((of, dir)) = &*taken {
            return (of == git_dir).then(|| dir.path().to_path_buf());
        }
        // Next to the jobs' scratch directories, so links stay on one filesystem.
        let dir = tempfile::Builder::new()
            .prefix("runthrough-objects-")
            .tempdir()
            .ok()?;
        in_parallel(files, |rel| {
            copy_entry(&git_dir.join(rel), &dir.path().join(rel))
        })
        .ok()?;
        let path = dir.path().to_path_buf();
        *taken = Some((git_dir.to_path_buf(), dir));
        Some(path)
    }
}

/// How one file reaches the copy.
enum Place {
    Copy,
    /// A shareable file: linked to this snapshot file when it holds the same
    /// thing, else copied fresh (never through a link an earlier run left).
    Share(Option<PathBuf>),
}

/// The repository's files relative to `root`, tracked and untracked, as `git
/// ls-files` lists them (leaving out ignored files with `exclude_ignored`);
/// every file under `root` when git cannot list them.
pub fn list_repo_files(root: &Path, exclude_ignored: bool) -> Vec<PathBuf> {
    let mut ls = Command::new("git");
    ls.arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others"]);
    if exclude_ignored {
        ls.arg("--exclude-standard");
    }
    match ls.output() {
        Ok(o) if o.status.success() => o
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
            .collect(),
        _ => walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
            .collect(),
    }
}

/// Copy the local repository into `dest` (emulates `actions/checkout`).
/// Tracked and untracked files are copied, plus `.git`; ignored files only
/// when `use_gitignore` is false. With `committed`, only `.git` is copied and
/// the HEAD commit's files are checked out of it, so uncommitted and untracked
/// changes stay behind. Objects come from `objects`.
pub fn copy_repo(
    src: &Path,
    dest: &Path,
    use_gitignore: bool,
    committed: bool,
    objects: &ObjectSnapshot,
) -> Result<()> {
    let git_dir = src.join(".git");
    if committed && !git_dir.is_dir() {
        bail!(
            "--committed needs the repository's .git directory ({} is not one; worktrees and submodules are not supported)",
            git_dir.display()
        );
    }
    let files = if committed {
        Vec::new()
    } else {
        list_repo_files(src, use_gitignore)
    };
    let mut plan: Vec<(PathBuf, Place)> = files.into_iter().map(|f| (f, Place::Copy)).collect();
    if git_dir.is_dir() {
        let (shared, own): (Vec<PathBuf>, Vec<PathBuf>) = walkdir::WalkDir::new(&git_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() || e.file_type().is_symlink())
            .filter_map(|e| e.path().strip_prefix(&git_dir).ok().map(Path::to_path_buf))
            .partition(|rel| immutable(rel));
        let snapshot = objects.dir(&git_dir, &shared);
        plan.extend(
            own.into_iter()
                .map(|rel| (Path::new(".git").join(rel), Place::Copy)),
        );
        plan.extend(shared.into_iter().map(|rel| {
            // An object written since the snapshot is copied from the repository.
            let same = |f: &PathBuf| {
                let len = |p: &Path| std::fs::symlink_metadata(p).map(|m| m.len()).ok();
                len(f).is_some() && len(f) == len(&git_dir.join(&rel))
            };
            let linked = snapshot.as_ref().map(|d| d.join(&rel)).filter(same);
            (Path::new(".git").join(rel), Place::Share(linked))
        }));
    }
    in_parallel(&plan, |(rel, place)| {
        let (from, to) = (src.join(rel), dest.join(rel));
        match place {
            Place::Copy => copy_entry(&from, &to),
            Place::Share(linked) => {
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let _ = std::fs::remove_file(&to);
                match linked {
                    Some(shared) if std::fs::hard_link(shared, &to).is_ok() => Ok(()),
                    _ => copy_entry(&from, &to),
                }
            }
        }
    })?;
    if committed {
        let out = Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["reset", "--quiet", "--hard", "HEAD"])
            .output()?;
        if !out.status.success() {
            bail!(
                "--committed: checking out the HEAD commit failed (no commit yet?): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// The files of `root`'s HEAD commit, exported into a new temporary directory
/// (`git archive`), for `--committed` to read workflows as the commit has them.
pub fn export_head(root: &Path) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("runthrough-head")
        .tempdir()?;
    let mut git = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["archive", "--format=tar", "HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running git archive")?;
    let tar = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(dir.path())
        .stdin(git.stdout.take().expect("piped stdout"))
        .status()
        .context("running tar")?;
    let out = git.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "--committed needs a HEAD commit in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if !tar.success() {
        bail!("extracting the HEAD commit of {} failed", root.display());
    }
    Ok(dir)
}

/// How many changes `git status` shows in `root` (uncommitted, untracked);
/// none when git cannot tell.
pub fn uncommitted_changes(root: &Path) -> usize {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        // One line per change; a path with a newline is quoted by git.
        .map_or(0, |o| {
            o.stdout
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .count()
        })
}

/// `f` for every item, on a few threads: copying file by file is bound by
/// per-file system calls, not bandwidth.
fn in_parallel<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| -> Result<()> {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(i) else {
                            return Ok(());
                        };
                        f(item)?;
                    }
                })
            })
            .collect();
        handles.into_iter().try_for_each(|h| {
            h.join()
                .unwrap_or_else(|_| bail!("a copying thread panicked"))
        })
    })
}

fn copy_entry(from: &Path, to: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(from) {
        Ok(m) => m,
        Err(_) => return Ok(()), // listed by git but deleted in the working tree
    };
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = std::fs::read_link(from)?;
            let _ = std::fs::remove_file(to);
            std::os::unix::fs::symlink(target, to)?;
        }
    } else if meta.is_file() {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

/// Environment variable name for an action input: `INPUT_<NAME>`.
pub fn input_env_name(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_in_progress_holds_up_only_its_own_action() {
        let cache = tempfile::tempdir().unwrap();
        let cached = cache.path().join("o__cached@v1");
        std::fs::create_dir_all(cached.join(".git")).unwrap();
        let busy = fetch_lock(&cache.path().join("o__busy@v1"));
        let _cloning = busy.lock().unwrap();
        let host = GithubHost {
            instance: "github.com".into(),
            replace_with_github_com: vec![],
            github_com_token: None,
            token: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = cache.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_remote(&dir, &host, "o", "cached", "v1", true).unwrap());
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a cached action waited for another action's clone");
        assert_eq!(got, cached);
        assert!(Arc::ptr_eq(
            &busy,
            &fetch_lock(&cache.path().join("o__busy@v1"))
        ));
    }

    #[test]
    fn copy_repo_copies_tracked_untracked_and_git_files() {
        let src = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(src.path(), &["init", "--quiet"]);
        for n in 0..200 {
            let f = src.path().join(format!("d{}/f{n}.txt", n % 7));
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, format!("{n}")).unwrap();
        }
        std::fs::write(src.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(src.path().join("ignored.txt"), "x").unwrap();
        git(src.path(), &["add", "."]);
        git(src.path(), &["commit", "--quiet", "-m", "c"]);
        git(src.path(), &["repack", "-a", "-d", "-q"]);
        std::fs::write(src.path().join("untracked.txt"), "u").unwrap();

        let objects = ObjectSnapshot::default();
        let first = tempfile::tempdir().unwrap();
        copy_repo(src.path(), first.path(), true, false, &objects).unwrap();
        for n in 0..200 {
            let f = first.path().join(format!("d{}/f{n}.txt", n % 7));
            assert_eq!(std::fs::read_to_string(f).unwrap(), format!("{n}"));
        }
        assert!(first.path().join("untracked.txt").is_file());
        assert!(!first.path().join("ignored.txt").exists());
        assert_eq!(git(first.path(), &["log", "--format=%s"]), "c");

        // A commit made during the run reaches the next checkout too.
        std::fs::write(src.path().join("late.txt"), "l").unwrap();
        git(src.path(), &["add", "late.txt"]);
        git(src.path(), &["commit", "--quiet", "-m", "late"]);
        let second = tempfile::tempdir().unwrap();
        copy_repo(src.path(), second.path(), true, false, &objects).unwrap();
        assert_eq!(git(second.path(), &["log", "--format=%s"]), "late\nc");
        git(second.path(), &["fsck", "--no-progress"]);

        // Checkouts share the snapshot's packs; the user's own files are never linked.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let pack_dir = src.path().join(".git/objects/pack");
            let pack = std::fs::read_dir(&pack_dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .find(|n| n.to_string_lossy().ends_with(".pack"))
                .unwrap();
            let rel = Path::new(".git/objects/pack").join(pack);
            let meta = |root: &Path| std::fs::metadata(root.join(&rel)).unwrap();
            assert_eq!(meta(first.path()).ino(), meta(second.path()).ino());
            assert_ne!(meta(first.path()).ino(), meta(src.path()).ino());
            assert_eq!(meta(src.path()).nlink(), 1);
        }
    }

    #[test]
    fn committed_copy_leaves_uncommitted_changes_behind() {
        let src = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(src.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "--quiet"]);
        let objects = ObjectSnapshot::default();
        let empty = tempfile::tempdir().unwrap();
        assert!(copy_repo(src.path(), empty.path(), true, true, &objects).is_err());

        std::fs::write(src.path().join("a.txt"), "committed").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "--quiet", "-m", "c"]);
        std::fs::write(src.path().join("a.txt"), "edited").unwrap();
        std::fs::write(src.path().join("new.txt"), "untracked").unwrap();
        assert_eq!(uncommitted_changes(src.path()), 2);

        let dest = tempfile::tempdir().unwrap();
        copy_repo(
            src.path(),
            dest.path(),
            true,
            true,
            &ObjectSnapshot::default(),
        )
        .unwrap();
        let a = std::fs::read_to_string(dest.path().join("a.txt")).unwrap();
        assert_eq!(a, "committed");
        assert!(!dest.path().join("new.txt").exists());
        assert_eq!(uncommitted_changes(dest.path()), 0);
    }

    #[test]
    fn only_content_addressed_git_files_are_shared() {
        for rel in [
            "objects/ab/cdef0123",
            "objects/pack/pack-1a2b.pack",
            "objects/pack/pack-1a2b.idx",
            "lfs/objects/aa/bb/aabbcc",
        ] {
            assert!(immutable(Path::new(rel)), "{rel}");
        }
        for rel in [
            "index",
            "HEAD",
            "config",
            "refs/heads/main",
            "objects/info/commit-graph",
            "objects/info/packs",
            "objects/pack/multi-pack-index",
            "objects/xyz/abc",
        ] {
            assert!(!immutable(Path::new(rel)), "{rel}");
        }
    }

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"x-access-token:abc"), "eC1hY2Nlc3MtdG9rZW46YWJj");
    }

    #[test]
    fn ghe_sources() {
        let host = GithubHost {
            instance: "ghe.example.com".into(),
            replace_with_github_com: vec!["actions/setup-node".into()],
            github_com_token: Some("t".into()),
            token: Some("ghe".into()),
        };
        assert_eq!(
            host.source("my", "action"),
            (
                "https://ghe.example.com/my/action".into(),
                Some("ghe".into())
            )
        );
        assert_eq!(
            host.source("Actions", "setup-node"),
            (
                "https://github.com/Actions/setup-node".into(),
                Some("t".into())
            )
        );
    }

    #[test]
    fn parse_refs() {
        assert_eq!(
            ActionRef::parse("./.github/actions/x").unwrap(),
            ActionRef::Local("./.github/actions/x".into())
        );
        assert_eq!(
            ActionRef::parse("actions/cache/restore@v4").unwrap(),
            ActionRef::Remote {
                owner: "actions".into(),
                repo: "cache".into(),
                path: "restore".into(),
                git_ref: "v4".into()
            }
        );
        assert!(ActionRef::parse("actions/checkout@v4")
            .unwrap()
            .is_checkout());
        for (uses, installs) in [
            ("actions/setup-python@v6", true),
            ("ruby/Setup-Ruby@v1", true),
            ("actions/checkout@v4", false),
            ("pnpm/action-setup@v4", false),
            ("./.github/actions/setup-env", false),
        ] {
            assert_eq!(
                ActionRef::parse(uses).unwrap().installs_tools(),
                installs,
                "{uses}"
            );
        }
        assert_eq!(
            ActionRef::parse("docker://alpine:3").unwrap(),
            ActionRef::Docker("alpine:3".into())
        );
        assert!(ActionRef::parse("actions/checkout").is_err());
        assert!(is_commit_sha("a26af69be951a213d495a4c3e4e4022e16d87065"));
        assert!(is_commit_sha(&"ab".repeat(32)));
        assert!(!is_commit_sha("v4"));
        assert!(!is_commit_sha("a26af69")); // a short SHA may also be a branch name
        assert!(!is_commit_sha(&"g".repeat(40)));
        assert_eq!(
            input_env_name("node version"),
            "INPUT_NODE VERSION".replace(' ', "_")
        );
    }
}
