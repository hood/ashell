use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::git::{self, Freshness, Head, Repo};
use crate::protocol::Request;
use crate::config::{self, Config, Mark, Segment};
use crate::render::{Line, Shell};
use crate::toolchain::Toolchains;

const MAX_REPOS: usize = 32;

pub struct Engine {
    oneshot: bool,
    repos: Mutex<HashMap<PathBuf, Arc<Repo>>>,
    tick: AtomicU64,
    toolchains: Arc<Toolchains>,
    config: Arc<Config>,
    /// Set when the config file is broken (defaults in use).
    config_error: Option<String>,
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
    pub fn new(oneshot: bool, config: Config, config_error: Option<String>) -> Self {
        Self {
            oneshot,
            repos: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
            toolchains: Arc::default(),
            config: Arc::new(config),
            config_error,
        }
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

    /// `since`: when the prompt was requested; bounds freshness of unwatched
    /// state. `deadline`: total wait for async segments (shared).
    pub fn render(&self, req: &Request, deadline: Duration, since: Instant, shell: Shell) -> Rendered {
        let until = Instant::now().checked_add(deadline);
        let cfg = &*self.config;
        let mut timings = Vec::new();
        let mut t = Instant::now();
        let mut lap = |name, timings: &mut Vec<_>| {
            let now = Instant::now();
            timings.push((name, now - t));
            t = now;
        };
        let uses = |s: Segment| cfg.left.contains(&s) || cfg.right.contains(&s);

        let cwd = Path::new(&req.cwd);
        let need_repo = uses(Segment::Git) || (uses(Segment::Directory) && cfg.directory.repo_relative);
        let repo = need_repo.then(|| git::discover(cwd).map(|l| self.repo(l))).flatten();
        lap("discover", &mut timings);
        // Kick toolchain revalidation first so it overlaps the git wait.
        let tools = uses(Segment::Toolchains).then(|| self.toolchains.start(req, since, &cfg.toolchains));
        lap("toolchains kick", &mut timings);

        let mut pending = false;
        let new_line = || Line::new(shell, &cfg.separator, &cfg.padding);
        let mut lines = [new_line(), new_line()];
        if cfg.add_newline {
            lines[0].newline();
        }
        if self.config_error.is_some() {
            lines[0].begin();
            lines[0].seg_raw("1;33", None, "⚠ config"); // details: `ashell config check`
        }
        for (i, side) in [&cfg.left, &cfg.right].into_iter().enumerate() {
            let l = &mut lines[i];
            for seg in side {
                l.begin();
                match seg {
                    Segment::Directory => {
                        let r = repo.as_deref().filter(|_| cfg.directory.repo_relative);
                        let home = req.env("HOME").map(str::to_owned).or_else(|| std::env::var("HOME").ok());
                        directory(l, &cfg.directory, cwd, home.as_deref(), r);
                        lap("directory", &mut timings);
                    }
                    Segment::Git => {
                        if let Some(r) = &repo {
                            let left_time =
                                until.map_or(Duration::MAX, |u| u.saturating_duration_since(Instant::now()));
                            pending |= git_segment(l, &cfg.git, r, left_time, since);
                            lap("git", &mut timings);
                        }
                    }
                    Segment::Toolchains => {
                        if let Some(started) = &tools {
                            pending |= self.toolchains.render(l, started, req, until, &cfg.toolchains);
                            lap("toolchains", &mut timings);
                        }
                    }
                    Segment::Character => character(l, &cfg.character, req),
                    Segment::Duration => duration(l, &cfg.duration, req),
                    Segment::Status => status(l, &cfg.status, req),
                    Segment::Newline => l.newline(),
                    Segment::Text(t) => {
                        if !t.text.is_empty() {
                            l.seg(&t.style, &t.text);
                        }
                    }
                }
            }
            l.end();
        }
        let [mut left, right] = lines;
        if !left.buf.is_empty() {
            left.raw(" ");
        }
        lap("rest", &mut timings);

        Rendered { left: left.buf, right: right.buf, pending, watchable: repo.is_some(), timings }
    }

    pub fn git_compute_time(&self, cwd: &Path) -> Option<Duration> {
        let loc = git::discover(cwd)?;
        self.repos.lock().unwrap().get(&loc.workdir)?.last_compute()
    }
}

fn directory(l: &mut Line, cfg: &config::Directory, cwd: &Path, home: Option<&str>, repo: Option<&Repo>) {
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
            Some(rel) if rel.as_os_str().is_empty() => cfg.home_symbol.clone(),
            Some(rel) => format!("{}/{}", cfg.home_symbol, rel.display()),
            None => cwd.display().to_string(),
        },
    };
    let parts: Vec<&str> = shown.split('/').collect();
    let keep = cfg.truncate;
    if keep > 0 && parts.len() > keep {
        l.seg(&cfg.style, &format!("{}{}", cfg.truncation_symbol, parts[parts.len() - keep..].join("/")));
    } else {
        l.seg(&cfg.style, &shown);
    }
}

/// Returns true when the status part is still pending.
fn git_segment(l: &mut Line, cfg: &config::Git, repo: &Arc<Repo>, deadline: Duration, since: Instant) -> bool {
    match repo.head() {
        Some(Head::Branch(b)) => l.seg(&cfg.branch_style, &format!("{}{b}", cfg.branch_symbol)),
        Some(Head::Detached(h)) => {
            l.seg(&cfg.branch_style, &format!("{}{}{h}", cfg.branch_symbol, cfg.detached_prefix))
        }
        None => return false,
    }
    let (st, fresh) = repo.status(deadline, since);
    let stale = !matches!(fresh, Freshness::Fresh);
    let Some(st) = st else {
        l.seg(&cfg.stale_style, &cfg.pending_symbol);
        return true;
    };
    let stash = repo.stash_count() as u32;
    let marks = [
        (Mark::Conflicted, st.conflicted, &cfg.conflicted, &cfg.conflicted_style),
        (Mark::Staged, st.staged, &cfg.staged, &cfg.staged_style),
        (Mark::Modified, st.modified, &cfg.modified, &cfg.modified_style),
        (Mark::Untracked, st.untracked, &cfg.untracked, &cfg.untracked_style),
        (Mark::Ahead, st.ahead, &cfg.ahead, &cfg.sync_style),
        (Mark::Behind, st.behind, &cfg.behind, &cfg.sync_style),
        (Mark::Stash, stash, &cfg.stash, &cfg.stash_style),
    ];
    let mut first = true;
    for (mark, n, sym, style) in marks {
        if n == 0 {
            continue;
        }
        let style = if stale { &cfg.stale_style } else { style };
        let text = if cfg.hide_counts.contains(&mark) { sym.clone() } else { format!("{sym}{n}") };
        if first { l.seg(style, &text) } else { l.styled(style, &text) }
        first = false;
    }
    stale
}

fn character(l: &mut Line, cfg: &config::Character, req: &Request) {
    let (sym, style) = match (req.keymap == "vicmd", req.status == 0) {
        (true, ok) => (&cfg.vicmd_symbol, if ok { &cfg.success_style } else { &cfg.error_style }),
        (false, true) => (&cfg.success_symbol, &cfg.success_style),
        (false, false) => (&cfg.error_symbol, &cfg.error_style),
    };
    l.seg(style, sym);
}

fn duration(l: &mut Line, cfg: &config::Duration, req: &Request) {
    let Some(ms) = req.duration_ms.filter(|&ms| ms >= cfg.min_ms) else { return };
    let s = ms / 1000;
    let text = match s {
        0..60 => format!("{}.{}s", s, (ms % 1000) / 100),
        60..3600 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    };
    l.seg(&cfg.style, &text);
}

fn status(l: &mut Line, cfg: &config::Status, req: &Request) {
    if req.status != 0 {
        let text = match req.status {
            130 => "INT".to_owned(),
            n if n > 128 => format!("SIG{}", n - 128),
            n => n.to_string(),
        };
        l.seg(&cfg.style, &text);
    }
}
