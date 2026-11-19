use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::git::{self, Freshness, Head, Repo};
use crate::protocol::Request;
use crate::render::{Line, Shell, sgr};

const MAX_REPOS: usize = 32;
const BRANCH_GLYPH: &str = "\u{e0a0} ";

pub struct Engine {
    oneshot: bool,
    repos: Mutex<HashMap<PathBuf, Arc<Repo>>>,
    tick: AtomicU64,
}

pub struct Rendered {
    pub left: String,
    pub right: String,
    /// Some segment is still computing; a patch frame will follow.
    pub pending: bool,
    /// A git repo is involved, so late FS events may still change the prompt.
    pub watchable: bool,
    pub timings: Vec<(&'static str, Duration)>,
}

impl Engine {
    pub fn new(oneshot: bool) -> Self {
        Self { oneshot, repos: Mutex::new(HashMap::new()), tick: AtomicU64::new(0) }
    }

    fn repo(&self, loc: git::Location) -> Arc<Repo> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        let mut repos = self.repos.lock().unwrap();
        if let Some(r) = repos.get(&loc.workdir) {
            r.last_used.store(tick, Ordering::Relaxed);
            return r.clone();
        }
        if repos.len() >= MAX_REPOS {
            let oldest = repos
                .iter()
                .min_by_key(|(_, r)| r.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                repos.remove(&k);
            }
        }
        let key = loc.workdir.clone();
        let r = Repo::new(loc, self.oneshot);
        r.last_used.store(tick, Ordering::Relaxed);
        if !self.oneshot {
            let w = r.clone();
            std::thread::spawn(move || w.watch());
        }
        repos.insert(key, r.clone());
        r
    }

    pub fn render(&self, req: &Request, deadline: Duration, shell: Shell) -> Rendered {
        let mut timings = Vec::new();
        let mut t = Instant::now();
        let mut lap = |name, timings: &mut Vec<_>| {
            let now = Instant::now();
            timings.push((name, now - t));
            t = now;
        };

        let cwd = Path::new(&req.cwd);
        let repo = git::discover(cwd).map(|l| self.repo(l));
        lap("discover", &mut timings);

        let mut left = Line::new(shell);
        let home = req.env("HOME").map(str::to_owned).or_else(|| std::env::var("HOME").ok());
        directory(&mut left, cwd, home.as_deref(), repo.as_deref());
        lap("directory", &mut timings);

        let mut pending = false;
        if let Some(r) = &repo {
            pending = git_segment(&mut left, r, deadline);
            lap("git", &mut timings);
        }

        character(&mut left, req);
        left.raw(" ");

        let mut right = Line::new(shell);
        duration(&mut right, req);
        status(&mut right, req);
        lap("right", &mut timings);

        Rendered { left: left.buf, right: right.buf, pending, watchable: repo.is_some(), timings }
    }

    pub fn git_compute_time(&self, cwd: &Path) -> Option<Duration> {
        let loc = git::discover(cwd)?;
        self.repos.lock().unwrap().get(&loc.workdir)?.last_compute()
    }
}

fn directory(l: &mut Line, cwd: &Path, home: Option<&str>, repo: Option<&Repo>) {
    const KEEP: usize = 4;
    let shown: String = match repo {
        Some(r) => {
            let name = r.loc.workdir.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            match cwd.strip_prefix(&r.loc.workdir) {
                Ok(rel) if rel.as_os_str().is_empty() => name.into_owned(),
                Ok(rel) => format!("{name}/{}", rel.display()),
                Err(_) => cwd.display().to_string(),
            }
        }
        None => match home.and_then(|h| cwd.strip_prefix(h).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => "~".into(),
            Some(rel) => format!("~/{}", rel.display()),
            None => cwd.display().to_string(),
        },
    };
    let parts: Vec<&str> = shown.split('/').collect();
    if parts.len() > KEEP {
        l.seg(sgr::DIR, &format!("…/{}", parts[parts.len() - KEEP..].join("/")));
    } else {
        l.seg(sgr::DIR, &shown);
    }
}

/// Returns true when the status part is still pending.
fn git_segment(l: &mut Line, repo: &Arc<Repo>, deadline: Duration) -> bool {
    match repo.head() {
        Some(Head::Branch(b)) => l.seg(sgr::BRANCH, &format!("{BRANCH_GLYPH}{b}")),
        Some(Head::Detached(h)) => l.seg(sgr::BRANCH, &format!("{BRANCH_GLYPH}@{h}")),
        None => return false,
    }
    let (st, fresh) = repo.status(deadline);
    let stale = !matches!(fresh, Freshness::Fresh);
    let Some(st) = st else {
        l.seg(sgr::DIM, "…");
        return true;
    };
    let mut parts: Vec<(&str, String)> = Vec::new();
    let mut add = |n: u32, sym: &str, color| {
        if n > 0 {
            parts.push((color, format!("{sym}{n}")));
        }
    };
    add(st.conflicted, "=", sgr::CONFLICT);
    add(st.staged, "+", sgr::STAGED);
    add(st.modified, "!", sgr::MODIFIED);
    add(st.untracked, "?", sgr::UNTRACKED);
    add(st.ahead, "⇡", sgr::SYNC);
    add(st.behind, "⇣", sgr::SYNC);
    let stash = repo.stash_count() as u32;
    add(stash, "*", sgr::STASH);
    for (i, (color, text)) in parts.iter().enumerate() {
        let c = if stale { sgr::DIM } else { color };
        if i == 0 { l.seg(c, text) } else { l.styled(c, text) }
    }
    stale
}

fn character(l: &mut Line, req: &Request) {
    let sym = if req.keymap == "vicmd" { "❮" } else { "❯" };
    l.seg(if req.status == 0 { sgr::OK } else { sgr::ERR }, sym);
}

fn duration(l: &mut Line, req: &Request) {
    let Some(ms) = req.duration_ms.filter(|&ms| ms >= 2000) else { return };
    let s = ms / 1000;
    let text = match s {
        0..60 => format!("{}.{}s", s, (ms % 1000) / 100),
        60..3600 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    };
    l.seg(sgr::DURATION, &text);
}

fn status(l: &mut Line, req: &Request) {
    if req.status != 0 {
        let text = match req.status {
            130 => "INT".to_owned(),
            n if n > 128 => format!("SIG{}", n - 128),
            n => n.to_string(),
        };
        l.seg(sgr::ERR, &text);
    }
}
