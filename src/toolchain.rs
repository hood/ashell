//! Toolchain segments (rust, node, python...): detected like Starship from
//! marker files in the current dir, versions read by running the tool once.
//!
//! The critical path only does cache lookups. A known version renders at
//! once and each prompt revalidates it in the background (stale while
//! revalidate: a change patches the prompt). The version is a function of a
//! `Key` (resolved binary + its stat, version pin files in cwd and ancestors,
//! global defaults, env vars), and the tool runs again only when that key
//! changes, or after `TTL` (in-place upgrades behind shims like rustup).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bus::BUS;
use crate::protocol::{ENV_DEPS, Request};
use crate::render::Line;

/// Re-run a tool at least this often, even if its key did not change.
const TTL: Duration = Duration::from_secs(30 * 60);
/// A version command running longer than this is killed (shows nothing).
const EXEC_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENTRIES: usize = 1024;
/// Without a command in between (empty enter), reuse a check this recent.
/// Commands are what change toolchains; this bounds changes made elsewhere.
const RECHECK_AFTER: Duration = Duration::from_secs(5);

/// Version pins understood by asdf/mise for every tool.
const COMMON_PINS: &[&str] = &[".tool-versions", "mise.toml", ".mise.toml"];

pub struct Tool {
    pub name: &'static str,
    symbol: &'static str,
    color: &'static str,
    files: &'static [&'static str],
    exts: &'static [&'static str],
    dirs: &'static [&'static str],
    /// Shown when any of these is set, even without marker files.
    detect_env: &'static [&'static str],
    /// Candidate binaries, first found on PATH wins.
    bins: &'static [&'static str],
    args: &'static [&'static str],
    parse: fn(&str) -> Option<String>,
    /// Files in cwd or any ancestor that select the version.
    pins: &'static [&'static str],
    /// Global defaults: (env var overriding the dir, default dir under HOME, file).
    globals: &'static [(&'static str, &'static str, &'static str)],
    /// Env vars that select the version (also sent by the shell).
    env: &'static [&'static str],
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "rust",
        symbol: "🦀 ",
        color: "1;31",
        files: &["Cargo.toml", "rust-toolchain", "rust-toolchain.toml"],
        exts: &["rs"],
        dirs: &[],
        detect_env: &[],
        bins: &["rustc"],
        args: &["--version"],
        parse: second_word,
        pins: &["rust-toolchain", "rust-toolchain.toml"],
        globals: &[("RUSTUP_HOME", ".rustup", "settings.toml"), ("RUSTUP_HOME", ".rustup", "toolchains")],
        env: &["RUSTUP_TOOLCHAIN", "RUSTUP_HOME"],
    },
    Tool {
        name: "node",
        symbol: "\u{e718} ",
        color: "1;32",
        files: &["package.json", ".node-version", ".nvmrc"],
        exts: &["js", "mjs", "cjs", "ts", "mts", "cts"],
        dirs: &["node_modules"],
        detect_env: &[],
        bins: &["node"],
        args: &["--version"],
        parse: first_word,
        pins: &[".node-version", ".nvmrc"],
        globals: &[],
        env: &["VOLTA_HOME"],
    },
    Tool {
        name: "python",
        symbol: "🐍 ",
        color: "1;33",
        files: &[
            "pyproject.toml",
            "requirements.txt",
            ".python-version",
            "Pipfile",
            "setup.py",
            "setup.cfg",
            "tox.ini",
            "__init__.py",
        ],
        exts: &["py", "ipynb"],
        dirs: &[],
        detect_env: &["VIRTUAL_ENV"],
        bins: &["python", "python3"],
        args: &["--version"],
        parse: second_word,
        pins: &[".python-version"],
        globals: &[("PYENV_ROOT", ".pyenv", "version")],
        env: &["VIRTUAL_ENV", "PYENV_VERSION", "CONDA_PREFIX"],
    },
    Tool {
        name: "go",
        symbol: "🐹 ",
        color: "1;36",
        files: &["go.mod", "go.sum", "go.work", "glide.yaml", "Gopkg.toml", ".go-version"],
        exts: &["go"],
        dirs: &["Godeps"],
        detect_env: &[],
        bins: &["go"],
        args: &["version"],
        parse: go_version,
        pins: &["go.mod", "go.work", ".go-version"],
        globals: &[],
        env: &["GOTOOLCHAIN", "GOROOT"],
    },
    Tool {
        name: "ruby",
        symbol: "💎 ",
        color: "1;31",
        files: &["Gemfile", ".ruby-version"],
        exts: &["rb"],
        dirs: &[],
        detect_env: &[],
        bins: &["ruby"],
        args: &["-v"],
        parse: ruby_version,
        pins: &[".ruby-version"],
        globals: &[("RBENV_ROOT", ".rbenv", "version")],
        env: &["RBENV_VERSION"],
    },
    Tool {
        name: "java",
        symbol: "☕ ",
        color: "31",
        files: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "build.sbt",
            ".java-version",
            ".sdkmanrc",
            "deps.edn",
            "project.clj",
        ],
        exts: &["java", "class", "jar", "gradle", "clj", "cljc"],
        dirs: &[],
        detect_env: &[],
        bins: &["java"],
        args: &["-version"],
        parse: java_version,
        pins: &[".java-version", ".sdkmanrc"],
        globals: &[],
        env: &["JAVA_HOME"],
    },
    Tool {
        name: "bun",
        symbol: "🥟 ",
        color: "1;31",
        files: &["bun.lockb", "bun.lock", "bunfig.toml"],
        exts: &[],
        dirs: &[],
        detect_env: &[],
        bins: &["bun"],
        args: &["--version"],
        parse: first_word,
        pins: &[],
        globals: &[],
        env: &[],
    },
    Tool {
        name: "deno",
        symbol: "🦕 ",
        color: "32",
        files: &["deno.json", "deno.jsonc", "deno.lock"],
        exts: &[],
        dirs: &[],
        detect_env: &[],
        bins: &["deno"],
        args: &["-V"],
        parse: second_word,
        pins: &[],
        globals: &[],
        env: &[],
    },
];

fn first_word(out: &str) -> Option<String> {
    let w = out.split_whitespace().next()?;
    Some(w.strip_prefix('v').unwrap_or(w).to_owned())
}

fn second_word(out: &str) -> Option<String> {
    out.split_whitespace().nth(1).map(str::to_owned)
}

/// `go version go1.22.0 darwin/arm64`
fn go_version(out: &str) -> Option<String> {
    out.split_whitespace().nth(2)?.strip_prefix("go").map(str::to_owned)
}

/// `ruby 3.3.0 (...)`, older `ruby 2.6.10p210 (...)`
fn ruby_version(out: &str) -> Option<String> {
    let v = out.split_whitespace().nth(1)?;
    Some(v.split('p').next().unwrap_or(v).to_owned())
}

/// `openjdk version "21.0.1" 2023-10-17` (on stderr)
fn java_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| l.contains("version \""))?;
    Some(line.split('"').nth(1)?.to_owned())
}

type FileKey = Option<(u64, i64, i64, u64)>;

fn file_key(p: &Path) -> FileKey {
    let m = fs::metadata(p).ok()?;
    Some((m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

/// Everything the version of a tool depends on.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    tool: usize,
    bin: Option<PathBuf>,
    bin_key: FileKey,
    pins: Vec<(PathBuf, FileKey)>,
    globals: Vec<FileKey>,
    env: Vec<Option<String>>,
}

/// Env as the shell sees it: request values for declared vars, else ours.
struct Env<'a>(&'a Request);

impl Env<'_> {
    fn get(&self, name: &str) -> Option<String> {
        if ENV_DEPS.contains(&name) && !self.0.env.is_empty() {
            self.0.env(name).map(str::to_owned)
        } else {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
    }
}

fn which(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(name)).find(|p| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
    })
}

fn key(tool: usize, cwd: &Path, env: &Env) -> Key {
    let t = &TOOLS[tool];
    let path = env.get("PATH").unwrap_or_default();
    let bin = t.bins.iter().find_map(|b| which(b, &path));
    let bin_key = bin.as_deref().and_then(file_key);
    let mut pins = Vec::new();
    for dir in cwd.ancestors() {
        for name in t.pins.iter().chain(COMMON_PINS) {
            let p = dir.join(name);
            if let k @ Some(_) = file_key(&p) {
                pins.push((p, k));
            }
        }
    }
    let home = env.get("HOME").map(PathBuf::from);
    let mut globals: Vec<FileKey> = t
        .globals
        .iter()
        .map(|(var, default, file)| {
            let dir = env.get(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(default)));
            dir.and_then(|d| file_key(&d.join(file)))
        })
        .collect();
    if let Some(h) = &home {
        globals.push(file_key(&h.join(".tool-versions")));
        globals.push(file_key(&h.join(".config/mise/config.toml")));
    }
    let env = t.env.iter().map(|v| env.get(v)).collect();
    Key { tool, bin, bin_key, pins, globals, env }
}

/// Runs the tool's version command as the shell would (its PATH and env).
fn run_version(tool: usize, key: &Key, cwd: &Path, req: &Request) -> Option<String> {
    let t = &TOOLS[tool];
    let bin = key.bin.as_ref()?;
    // macOS ships a /usr/bin/java stub that opens an install dialog.
    if t.name == "java"
        && bin == Path::new("/usr/bin/java")
        && Env(req).get("JAVA_HOME").is_none()
        && fs::read_dir("/Library/Java/JavaVirtualMachines").map_or(true, |mut d| d.next().is_none())
    {
        return None;
    }
    let mut cmd = Command::new(bin);
    cmd.args(t.args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if !req.env.is_empty() {
        for (i, var) in ENV_DEPS.iter().enumerate() {
            match req.env.get(i).filter(|v| !v.is_empty()) {
                Some(v) => cmd.env(var, v),
                None => cmd.env_remove(var),
            };
        }
    }
    let mut child = cmd.spawn().ok()?;
    let until = Instant::now() + EXEC_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(if out.stdout.is_empty() { &out.stderr } else { &out.stdout }).into_owned();
    (t.parse)(&text)
}

/// Tools detected in a dir, cached by the dir's stat (adding or removing an
/// entry bumps its mtime).
fn detect(names: &[OsString]) -> Vec<usize> {
    let mut found = vec![false; TOOLS.len()];
    for n in names {
        let Some(n) = n.to_str() else { continue };
        let ext = n.rsplit_once('.').map(|(_, e)| e);
        for (i, t) in TOOLS.iter().enumerate() {
            if !found[i]
                && (t.files.contains(&n) || t.dirs.contains(&n) || ext.is_some_and(|e| t.exts.contains(&e)))
            {
                found[i] = true;
            }
        }
    }
    (0..TOOLS.len()).filter(|&i| found[i]).collect()
}

#[derive(Default)]
struct Slot {
    /// Last known version; `None` inside: tool missing or unparsable.
    value: Option<Option<String>>,
    key: Option<Key>,
    validated: Option<Instant>,
    computing: bool,
}

#[derive(Default)]
struct Entry {
    slot: Mutex<Slot>,
    cv: Condvar,
}

type Detected = Arc<Vec<usize>>;

#[derive(Default)]
pub struct Toolchains {
    /// Tools detected per dir, with the dir's stat at detection.
    dirs: Mutex<HashMap<PathBuf, (FileKey, Detected)>>,
    /// Per (tool, cwd, env) shown value.
    entries: Mutex<HashMap<(usize, PathBuf, u64), Arc<Entry>>>,
    /// Versions by key, shared across dirs.
    versions: Mutex<HashMap<Key, (Option<String>, Instant)>>,
    /// Tool executions, for tests.
    pub runs: std::sync::atomic::AtomicU32,
}

/// Validations started for one prompt; `render` waits on them.
pub struct Started(Vec<(usize, Arc<Entry>)>);

impl Toolchains {
    fn detected(&self, cwd: &Path, req: &Request) -> Arc<Vec<usize>> {
        let k = file_key(cwd);
        let mut dirs = self.dirs.lock().unwrap();
        let tools = match dirs.get(cwd) {
            Some((dk, t)) if *dk == k && k.is_some() => t.clone(),
            _ => {
                let names: Vec<OsString> =
                    fs::read_dir(cwd).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
                let t = Arc::new(detect(&names));
                if dirs.len() >= MAX_ENTRIES {
                    dirs.clear();
                }
                dirs.insert(cwd.to_path_buf(), (k, t.clone()));
                t
            }
        };
        drop(dirs);
        let env = Env(req);
        let extra: Vec<usize> = (0..TOOLS.len())
            .filter(|i| !tools.contains(i) && TOOLS[*i].detect_env.iter().any(|v| env.get(v).is_some()))
            .collect();
        if extra.is_empty() {
            return tools;
        }
        let mut all = (*tools).clone();
        all.extend(extra);
        all.sort();
        Arc::new(all)
    }

    /// Kicks revalidation for the tools of `req.cwd` (key check: 40-70µs of
    /// stats, off the render path) if a command ran since the last check or
    /// it is older than `RECHECK_AFTER`. Render path: a stat of cwd plus map
    /// lookups.
    pub fn start(self: &Arc<Self>, req: &Request, since: Instant) -> Started {
        let cwd = PathBuf::from(&req.cwd);
        let tools = self.detected(&cwd, req);
        let mut h = std::hash::DefaultHasher::new();
        req.env.hash(&mut h);
        let env_hash = h.finish();
        let mut out = Vec::with_capacity(tools.len());
        for &tool in tools.iter() {
            let entry = {
                let mut m = self.entries.lock().unwrap();
                if m.len() >= MAX_ENTRIES {
                    m.clear();
                }
                m.entry((tool, cwd.clone(), env_hash)).or_default().clone()
            };
            let mut s = entry.slot.lock().unwrap();
            let stale = match s.validated {
                None => true,
                Some(v) => (req.ran_command() && v < since) || v.elapsed() > RECHECK_AFTER,
            };
            if !s.computing && stale {
                s.computing = true;
                let (me, e, req, cwd) = (self.clone(), entry.clone(), req.clone(), cwd.clone());
                POOL.run(move || me.validate(tool, &e, &req, &cwd));
            }
            drop(s);
            out.push((tool, entry));
        }
        Started(out)
    }

    fn validate(&self, tool: usize, entry: &Entry, req: &Request, cwd: &Path) {
        let k = key(tool, cwd, &Env(req));
        let cached = {
            let v = self.versions.lock().unwrap();
            v.get(&k).filter(|(_, at)| at.elapsed() < TTL).map(|(v, _)| v.clone())
        };
        let value = cached.unwrap_or_else(|| {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let v = run_version(tool, &k, cwd, req);
            let mut versions = self.versions.lock().unwrap();
            if versions.len() >= MAX_ENTRIES {
                versions.clear();
            }
            versions.insert(k.clone(), (v.clone(), Instant::now()));
            v
        });
        let mut s = entry.slot.lock().unwrap();
        s.value = Some(value);
        s.key = Some(k);
        s.validated = Some(Instant::now());
        s.computing = false;
        drop(s);
        entry.cv.notify_all();
        BUS.bump();
    }

    /// Renders started tools. Unknown versions are waited for until `until`
    /// (None: forever); known ones render at once while revalidating.
    /// Returns true if some validation is still running.
    pub fn render(&self, l: &mut Line, started: &Started, req: &Request, until: Option<Instant>) -> bool {
        let mut pending = false;
        for (tool, entry) in &started.0 {
            let mut s = entry.slot.lock().unwrap();
            while s.computing && s.value.is_none() {
                match until {
                    None => s = entry.cv.wait(s).unwrap(),
                    Some(u) => {
                        let left = u.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        s = entry.cv.wait_timeout(s, left).unwrap().0;
                    }
                }
            }
            pending |= s.computing;
            let t = &TOOLS[*tool];
            let version = s.value.as_ref().and_then(|v| v.as_deref());
            let mut text = match version {
                Some(v) => format!("{}v{v}", t.symbol),
                None => String::new(),
            };
            if t.name == "python"
                && let Some(venv) = venv_name(req)
            {
                if text.is_empty() {
                    text.push_str(t.symbol.trim_end());
                }
                text.push_str(&format!(" ({venv})"));
            }
            if !text.is_empty() {
                l.seg(t.color, &text);
            }
        }
        pending
    }
}

/// Long-lived workers for validations: a thread spawn per tool per prompt
/// costs ~20µs on the render path.
struct Pool(Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>);

const WORKERS: usize = 4;

static POOL: std::sync::LazyLock<Pool> = std::sync::LazyLock::new(|| {
    let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..WORKERS {
        let rx = rx.clone();
        std::thread::spawn(move || {
            loop {
                let job = rx.lock().unwrap().recv();
                match job {
                    Ok(job) => job(),
                    Err(_) => return,
                }
            }
        });
    }
    Pool(Mutex::new(tx))
});

impl Pool {
    fn run(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.0.lock().unwrap().send(Box::new(job));
    }
}

/// `VIRTUAL_ENV=/x/proj/.venv` shows as `proj`, `/x/envs/foo` as `foo`.
fn venv_name(req: &Request) -> Option<String> {
    let v = Env(req).get("VIRTUAL_ENV")?;
    let p = Path::new(&v);
    let base = p.file_name()?.to_string_lossy();
    let generic = matches!(&*base, ".venv" | "venv" | "env" | ".env" | "virtualenv");
    let name = if generic { p.parent()?.file_name()?.to_string_lossy() } else { base };
    Some(name.into_owned())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_vars_are_sent_by_the_shell() {
        for t in TOOLS {
            for v in t.env.iter().chain(t.detect_env) {
                assert!(ENV_DEPS.contains(v), "{v} ({}) missing from ENV_DEPS", t.name);
            }
        }
    }

    #[test]
    fn parses_versions() {
        assert_eq!(second_word("rustc 1.82.0 (f6e511eec 2024-10-15)\n").as_deref(), Some("1.82.0"));
        assert_eq!(second_word("rustc 1.84.0-nightly (abc 2024-11-01)").as_deref(), Some("1.84.0-nightly"));
        assert_eq!(first_word("v20.11.0\n").as_deref(), Some("20.11.0"));
        assert_eq!(first_word("1.1.38\n").as_deref(), Some("1.1.38"));
        assert_eq!(second_word("Python 3.12.1\n").as_deref(), Some("3.12.1"));
        assert_eq!(go_version("go version go1.22.0 darwin/arm64\n").as_deref(), Some("1.22.0"));
        assert_eq!(ruby_version("ruby 2.6.10p210 (2022-04-12 revision 67958) [universal.arm64e-darwin23]").as_deref(), Some("2.6.10"));
        assert_eq!(ruby_version("ruby 3.3.0 (2023-12-25 revision 5124f9ac75) [arm64-darwin23]").as_deref(), Some("3.3.0"));
        assert_eq!(
            java_version("openjdk version \"21.0.1\" 2023-10-17\nOpenJDK Runtime Environment").as_deref(),
            Some("21.0.1")
        );
        assert_eq!(java_version("java version \"1.8.0_392\"\n").as_deref(), Some("1.8.0_392"));
        assert_eq!(second_word("deno 1.40.0\n").as_deref(), Some("1.40.0"));
        assert_eq!(second_word(""), None);
    }

    #[test]
    fn detects_by_file_ext_dir() {
        let names = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let found = |v: &[&str]| detect(&names(v)).into_iter().map(|i| TOOLS[i].name).collect::<Vec<_>>();
        assert_eq!(found(&["Cargo.toml", "README.md"]), ["rust"]);
        assert_eq!(found(&["main.go", "x.py"]), ["python", "go"]);
        assert_eq!(found(&["node_modules", "build.sbt"]), ["node", "java"]);
        assert!(found(&["README.md", ".gitignore"]).is_empty());
    }

    struct Fixture {
        root: PathBuf,
        bin: PathBuf,
        proj: PathBuf,
    }

    impl Fixture {
        /// A project dir with Cargo.toml and a fake `rustc` on PATH that
        /// prints the version named by `rust-toolchain` (like a rustup shim).
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("ashell-tc-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let (bin, proj) = (root.join("bin"), root.join("proj/sub"));
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(&proj).unwrap();
            fs::write(proj.join("Cargo.toml"), "").unwrap();
            Self::script(&bin, "1.0.0");
            Self { root, bin, proj }
        }

        fn script(bin: &Path, default: &str) {
            use std::os::unix::fs::PermissionsExt;
            let p = bin.join("rustc");
            let body = format!(
                "#!/bin/sh\nd=$PWD\nwhile [ \"$d\" != / ]; do [ -f \"$d/rust-toolchain\" ] && {{ echo \"rustc $(cat \"$d/rust-toolchain\") (x)\"; exit; }}; d=$(dirname \"$d\"); done\necho \"rustc {default} (x)\"\n"
            );
            let tmp = bin.join(".rustc.tmp");
            fs::write(&tmp, body).unwrap();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            fs::rename(tmp, p).unwrap();
        }

        fn req(&self, path_first: &Path) -> Request {
            let mut env: Vec<String> = ENV_DEPS.iter().map(|_| String::new()).collect();
            let i = ENV_DEPS.iter().position(|v| *v == "PATH").unwrap();
            env[i] = format!("{}:/usr/bin:/bin", path_first.display());
            Request {
                seq: 1,
                cwd: self.proj.to_string_lossy().into_owned(),
                cols: 80,
                status: 0,
                duration_ms: Some(1), // a command ran: revalidate
                keymap: "main".into(),
                env,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// The prompt after revalidation settles (what the patch frame shows).
    fn shown(tc: &Arc<Toolchains>, req: &Request) -> String {
        let started = tc.start(req, Instant::now());
        for (_, e) in &started.0 {
            let mut s = e.slot.lock().unwrap();
            while s.computing {
                s = e.cv.wait(s).unwrap();
            }
        }
        let mut l = Line::new(crate::render::Shell::Plain);
        assert!(!tc.render(&mut l, &started, req, None));
        l.buf
    }

    #[test]
    fn versions_follow_pins_binary_and_path() {
        use std::sync::atomic::Ordering::Relaxed;
        let f = Fixture::new("pins");
        let tc = Arc::new(Toolchains::default());
        let req = f.req(&f.bin);
        assert!(shown(&tc, &req).contains("v1.0.0"));
        assert_eq!(tc.runs.load(Relaxed), 1);

        // Unchanged key: no re-run.
        assert!(shown(&tc, &req).contains("v1.0.0"));
        assert_eq!(tc.runs.load(Relaxed), 1, "cached by key");

        // Pin file in an ancestor selects another version.
        fs::write(f.root.join("proj/rust-toolchain"), "1.2.3").unwrap();
        assert!(shown(&tc, &req).contains("v1.2.3"));
        fs::remove_file(f.root.join("proj/rust-toolchain")).unwrap();
        assert!(shown(&tc, &req).contains("v1.0.0"));
        assert_eq!(tc.runs.load(Relaxed), 2, "back to a known key: cached");

        // Binary replaced in place (upgrade).
        Fixture::script(&f.bin, "2.0.0");
        assert!(shown(&tc, &req).contains("v2.0.0"));

        // Another binary earlier on PATH.
        let other = f.root.join("other");
        fs::create_dir_all(&other).unwrap();
        Fixture::script(&other, "3.0.0");
        let req2 = f.req(&other);
        assert!(shown(&tc, &req2).contains("v3.0.0"));

        // Missing tool: nothing shown.
        let req3 = f.req(&f.root.join("nope"));
        assert!(!shown(&tc, &req3).contains("🦀"));

        // Marker file removed: tool not detected anymore.
        fs::remove_file(f.proj.join("Cargo.toml")).unwrap();
        assert!(shown(&tc, &req).is_empty());
    }

    #[test]
    fn venv_names() {
        let req = |v: &str| {
            let mut env: Vec<String> = ENV_DEPS.iter().map(|_| String::new()).collect();
            env[ENV_DEPS.iter().position(|x| *x == "VIRTUAL_ENV").unwrap()] = v.into();
            Request { seq: 1, cwd: "/".into(), cols: 80, status: 0, duration_ms: None, keymap: "main".into(), env }
        };
        assert_eq!(venv_name(&req("/x/proj/.venv")).as_deref(), Some("proj"));
        assert_eq!(venv_name(&req("/x/envs/foo")).as_deref(), Some("foo"));
        assert_eq!(venv_name(&req("")), None);
    }
}
