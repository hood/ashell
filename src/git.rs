//! Git state. HEAD is read from disk every prompt (µs). Status is computed off
//! the critical path, cached per repo, and invalidated by an FS watcher.
//!
//! Status runs in-process via gitoxide (`gitnative`); `ASHELL_GIT=cli` falls
//! back to `git --no-optional-locks status`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::bus::BUS;
use crate::untracked;

pub struct Location {
    pub workdir: PathBuf,
    pub gitdir: PathBuf,
    /// Shared dir for linked worktrees (holds refs, stash); equals gitdir otherwise.
    pub commondir: PathBuf,
}

/// Walks up from `cwd` looking for `.git` (dir, or file for worktrees/submodules).
pub fn discover(cwd: &Path) -> Option<Location> {
    for dir in cwd.ancestors() {
        let dotgit = dir.join(".git");
        let Ok(meta) = fs::metadata(&dotgit) else { continue };
        let gitdir = if meta.is_dir() {
            dotgit
        } else {
            let s = fs::read_to_string(&dotgit).ok()?;
            let p = Path::new(s.strip_prefix("gitdir:")?.trim());
            if p.is_absolute() { p.to_path_buf() } else { dir.join(p) }
        };
        if fs::symlink_metadata(gitdir.join("HEAD")).is_err() {
            continue; // not a usable repo (git skips these too)
        }
        let commondir = fs::read_to_string(gitdir.join("commondir"))
            .ok()
            .map(|c| gitdir.join(c.trim()))
            .unwrap_or_else(|| gitdir.clone());
        return Some(Location { workdir: dir.to_path_buf(), gitdir, commondir });
    }
    None
}

#[derive(Clone)]
pub enum Head {
    Branch(String),
    Detached(String),
}

fn read_head(gitdir: &Path) -> Option<Head> {
    let s = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let s = s.trim();
    Some(match s.strip_prefix("ref: ") {
        Some(r) => Head::Branch(r.strip_prefix("refs/heads/").unwrap_or(r).to_owned()),
        None => Head::Detached(s.get(..7).unwrap_or(s).to_owned()),
    })
}

fn read_stash_count(commondir: &Path) -> usize {
    fs::read(commondir.join("logs/refs/stash"))
        .map(|b| b.iter().filter(|&&c| c == b'\n').count())
        .unwrap_or(0)
}

/// Identity of a file version: (ino, mtime ns, size). `stat` is ~30x cheaper
/// than `open` on machines with endpoint security hooks, and git updates refs
/// via rename, so the inode changes on every write.
type FileKey = Option<(u64, i128, u64)>;

fn file_key(p: &Path) -> FileKey {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(p).ok()?;
    Some((m.ino(), m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128, m.size()))
}

/// Memoizes a value derived from one file, revalidated by `stat`.
struct StatCache<T>(Mutex<Option<(FileKey, T)>>);

impl<T: Clone> StatCache<T> {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn get(&self, path: &Path, load: impl FnOnce() -> T) -> T {
        let key = file_key(path);
        let mut g = self.0.lock().unwrap();
        if let Some((k, v)) = &*g
            && *k == key
        {
            return v.clone();
        }
        let v = load();
        *g = Some((key, v.clone()));
        v
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Status {
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub ahead: u32,
    pub behind: u32,
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub fn parse_porcelain_v2(out: &[u8]) -> Status {
    let mut st = Status::default();
    let mut it = out.split(|&b| b == 0);
    while let Some(e) = it.next() {
        match e {
            [b'#', b' ', rest @ ..] => {
                if let Some(ab) = rest.strip_prefix(b"branch.ab ") {
                    let ab = String::from_utf8_lossy(ab);
                    for t in ab.split(' ') {
                        if let Some(n) = t.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = t.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
            }
            [k @ (b'1' | b'2'), b' ', x, y, ..] => {
                st.staged += (*x != b'.') as u32;
                st.modified += (*y != b'.') as u32;
                if *k == b'2' {
                    it.next(); // rename/copy origin path
                }
            }
            [b'u', b' ', ..] => st.conflicted += 1,
            [b'?', b' ', ..] => st.untracked += 1,
            _ => {}
        }
    }
    st
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Native,
    Cli,
}

impl Backend {
    pub fn from_env() -> Self {
        match std::env::var("ASHELL_GIT").as_deref() {
            Ok("cli") => Self::Cli,
            _ => Self::Native,
        }
    }
}

pub fn compute_status(workdir: &Path, cache: &mut untracked::Cache) -> Option<Status> {
    compute_status_with(workdir, Backend::from_env(), cache)
}

pub fn compute_status_with(workdir: &Path, backend: Backend, cache: &mut untracked::Cache) -> Option<Status> {
    match backend {
        Backend::Native => crate::gitnative::status(workdir, cache),
        Backend::Cli => cli_status(workdir),
    }
}

pub(crate) fn cli_status(workdir: &Path) -> Option<Status> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    out.status.success().then(|| parse_porcelain_v2(&out.stdout))
}

pub struct Cached {
    /// Watcher generation the value was computed at.
    pub generation: u64,
    /// When the computation started: without a watcher, a value is fresh for
    /// a prompt only if computed after that prompt was requested.
    pub started: Instant,
    pub status: Arc<Status>,
    pub took: Duration,
}

#[derive(Default)]
struct Inner {
    cached: Option<Cached>,
    computing: bool,
}

pub struct Repo {
    pub loc: Location,
    generation: AtomicU64,
    inner: Mutex<Inner>,
    cv: Condvar,
    pub last_used: AtomicU64,
    head: StatCache<Option<Head>>,
    /// Untracked-scan state, shared by the recompute and warm-up threads.
    untracked: Arc<Mutex<untracked::Cache>>,
    stash: StatCache<usize>,
    /// One-shot mode: no cache, compute synchronously.
    oneshot: bool,
    watching: AtomicBool,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

pub enum Freshness {
    Fresh,
    Stale,
    Missing,
}

impl Repo {
    pub fn new(loc: Location, oneshot: bool) -> Arc<Self> {
        Arc::new(Self {
            loc,
            oneshot,
            generation: AtomicU64::new(0),
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
            last_used: AtomicU64::new(0),
            head: StatCache::new(),
            untracked: Arc::new(Mutex::new(untracked::Cache::default())),
            stash: StatCache::new(),
            watching: AtomicBool::new(false),
            watcher: Mutex::new(None),
        })
    }

    /// Starts the FS watcher (can be slow with inotify, so call it off the
    /// render path). Until it runs, cached values are never trusted as fresh.
    pub fn watch(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let objects = self.loc.commondir.join("objects");
        let logs = self.loc.commondir.join("logs");
        let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let relevant = ev.paths.iter().any(|p| {
                !p.starts_with(&objects)
                    && !p.starts_with(&logs)
                    && p.extension().is_none_or(|e| e != "lock")
            });
            if let (true, Some(repo)) = (relevant, weak.upgrade()) {
                repo.invalidate();
            }
        });
        let Ok(mut w) = w else { return };
        if w.watch(&self.loc.workdir, RecursiveMode::Recursive).is_err() {
            return;
        }
        if !self.loc.gitdir.starts_with(&self.loc.workdir) {
            let _ = w.watch(&self.loc.gitdir, RecursiveMode::Recursive);
        }
        *self.watcher.lock().unwrap() = Some(w);
        self.watching.store(true, Ordering::Release);
    }

    pub fn head(&self) -> Option<Head> {
        let gd = &self.loc.gitdir;
        self.head.get(&gd.join("HEAD"), || read_head(gd))
    }

    pub fn stash_count(&self) -> usize {
        let cd = &self.loc.commondir;
        self.stash.get(&cd.join("logs/refs/stash"), || read_stash_count(cd))
    }

    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        BUS.bump();
    }

    /// Returns the best known status for a prompt requested at `since`,
    /// kicking a recompute if it is stale and waiting at most `deadline` for it.
    pub fn status(self: &Arc<Self>, deadline: Duration, since: Instant) -> (Option<Arc<Status>>, Freshness) {
        if self.oneshot {
            // No warm cache in a one-shot process: git's on-disk untracked
            // cache beats a cold native scan (aports: 85ms vs 1.5s).
            let wd = &self.loc.workdir;
            return match cli_status(wd).or_else(|| compute_status(wd, &mut untracked::Cache::default())) {
                Some(s) => (Some(Arc::new(s)), Freshness::Fresh),
                None => (None, Freshness::Missing),
            };
        }
        let trusted = |c: &Cached| {
            if self.watching.load(Ordering::Acquire) {
                c.generation == self.generation.load(Ordering::Acquire)
            } else {
                c.started >= since
            }
        };
        let mut g = self.inner.lock().unwrap();
        if g.cached.as_ref().is_some_and(trusted) {
            return (g.cached.as_ref().map(|c| c.status.clone()), Freshness::Fresh);
        }
        if !g.computing {
            g.computing = true;
            let repo = self.clone();
            std::thread::spawn(move || repo.recompute());
        }
        if !deadline.is_zero() {
            let until = Instant::now() + deadline;
            while g.computing {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                g = self.cv.wait_timeout(g, left).unwrap().0;
            }
        }
        match &g.cached {
            Some(c) if trusted(c) => (Some(c.status.clone()), Freshness::Fresh),
            Some(c) => (Some(c.status.clone()), Freshness::Stale),
            None => (None, Freshness::Missing),
        }
    }

    /// Native status when its untracked cache is warm. While cold (first scan
    /// of aports: ~1.5s of readdir) answer with git, which has its own on-disk
    /// untracked cache, and warm ours in the background.
    fn status_now(&self) -> Option<Status> {
        let wd = &self.loc.workdir;
        match self.untracked.try_lock() {
            Ok(mut cache) if cache.is_warm() => return compute_status(wd, &mut cache),
            Ok(cache) => {
                drop(cache);
                let (wd2, c) = (wd.clone(), self.untracked.clone());
                std::thread::spawn(move || {
                    let mut cache = c.lock().unwrap();
                    if !cache.is_warm() {
                        compute_status(&wd2, &mut cache);
                    }
                });
            }
            Err(_) => {} // warming up
        }
        cli_status(wd).or_else(|| compute_status(wd, &mut self.untracked.lock().unwrap()))
    }

    pub fn last_compute(&self) -> Option<Duration> {
        self.inner.lock().unwrap().cached.as_ref().map(|c| c.took)
    }

    fn recompute(&self) {
        // Value is only as fresh as the generation observed before scanning.
        let generation = self.generation.load(Ordering::Acquire);
        let started = Instant::now();
        // Native can fail on exotic repos (reftable, sha256...): fall back to
        // git, then to "clean" so a broken repo never causes a retry loop.
        let st = self
            .status_now()
            .or_else(|| cli_status(&self.loc.workdir))
            .unwrap_or_default();
        let took = started.elapsed();
        let mut g = self.inner.lock().unwrap();
        g.cached = Some(Cached { generation, started, status: Arc::new(st), took });
        g.computing = false;
        drop(g);
        self.cv.notify_all();
        BUS.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_counts() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.ab +2 -1\0\
1 M. N... 100644 100644 100644 a b f1\0\
1 .M N... 100644 100644 100644 a b f2\0\
1 MM N... 100644 100644 100644 a b f3\0\
2 R. N... 100644 100644 100644 a b R100 new\0old\0\
u UU N... 1 2 3 4 a b c conf\0\
? u1\0? u2\0";
        let s = parse_porcelain_v2(out);
        assert_eq!(
            s,
            Status { staged: 3, modified: 2, untracked: 2, conflicted: 1, ahead: 2, behind: 1 }
        );
    }
}
