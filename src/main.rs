mod bus;
mod daemon;
mod engine;
mod git;
mod gitnative;
mod protocol;
mod render;
mod untracked;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use engine::Engine;
use gitnative::Changes;
use protocol::{ENV_DEPS, Request};
use render::Shell;

const ZSH_INIT: &str = include_str!("../shell/init.zsh");

const USAGE: &str = "\
ashell: fast daemon-backed shell prompt

usage:
  ashell init zsh                      print shell integration
  ashell daemon [--socket P] [--detach]
  ashell stop [--socket P]
  ashell prompt [--shell zsh|bash|plain] [--status N] [--duration MS] [--keymap K]
  ashell explain [--status N] [--duration MS]
  ashell socket-path
  ashell git-status [--backend native|cli] [--warm [--trust]]   counts + timing
  ashell git-watch    recompute on every FS event, print timings (dev)";

/// `ASHELL_DEBUG=1`: timings and decisions on stderr (`ASHELL_LOG=<file>`
/// keeps a detached daemon's).
pub static DEBUG: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("ASHELL_DEBUG").is_some());

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    let sock = || opt("--socket").map(PathBuf::from).unwrap_or_else(daemon::default_socket);

    let res = match args.first().map(String::as_str) {
        Some("init") => match args.get(1).map(String::as_str) {
            Some("zsh") => {
                let exe = std::env::current_exe().unwrap_or_else(|_| "ashell".into());
                let script = ZSH_INIT
                    .replace("@ASHELL_BIN@", &shell_quote(&exe.to_string_lossy()))
                    .replace("@ASHELL_SOCK@", &shell_quote(&sock().to_string_lossy()));
                print!("{script}");
                Ok(())
            }
            _ => Err("supported shells: zsh".into()),
        },
        Some("daemon") if flag("--detach") => daemon::spawn_detached(&sock()).map_err(|e| e.to_string()),
        Some("daemon") => daemon::run(&sock()).map_err(|e| e.to_string()),
        Some("stop") => daemon::stop(&sock()).map_err(|e| e.to_string()),
        Some("socket-path") => {
            println!("{}", sock().display());
            Ok(())
        }
        Some("prompt") => {
            let shell = opt("--shell").and_then(Shell::parse).unwrap_or(Shell::Plain);
            let r = Engine::new(true).render(&local_request(opt), Duration::MAX, Instant::now(), shell);
            match shell {
                Shell::Plain => println!("{}\n{}", r.left, r.right),
                _ => print!("{}\x1f{}", r.left, r.right),
            }
            Ok(())
        }
        Some("git-watch") => match git::discover(&std::env::current_dir().unwrap_or_default()) {
            Some(loc) => git::Repo::new(loc, false).watch_loop(),
            None => Err("not in a git repo".into()),
        },
        Some("git-status") => {
            let backend = match opt("--backend") {
                Some("cli") => git::Backend::Cli,
                _ => git::Backend::Native,
            };
            let cwd = std::env::current_dir().unwrap_or_default();
            match git::discover(&cwd) {
                Some(loc) => {
                    let mut state = gitnative::State::default();
                    if flag("--warm") {
                        git::compute_status_with(&loc.workdir, backend, &mut state, Changes::Unknown);
                        // --warm --trust: pretend a watcher saw no changes.
                        state.synced = flag("--trust");
                    }
                    let changes = if flag("--trust") { Changes::Known(Default::default()) } else { Changes::Unknown };
                    let t = Instant::now();
                    let st = git::compute_status_with(&loc.workdir, backend, &mut state, changes);
                    let took = t.elapsed();
                    if *DEBUG {
                        eprintln!("{:?} {:?}", state.stats, state.untracked.stats);
                    }
                    match st {
                        Some(s) => {
                            println!(
                                "staged={} modified={} untracked={} conflicted={} ahead={} behind={} took_us={}",
                                s.staged, s.modified, s.untracked, s.conflicted, s.ahead, s.behind,
                                took.as_micros()
                            );
                            Ok(())
                        }
                        None => Err("status failed".into()),
                    }
                }
                None => Err("not in a git repo".into()),
            }
        }
        Some("explain") => {
            explain(local_request(opt));
            Ok(())
        }
        _ => Err(USAGE.into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn local_request<'a>(opt: impl Fn(&str) -> Option<&'a str>) -> Request {
    Request {
        seq: 1,
        cwd: std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        cols: 80,
        status: opt("--status").and_then(|s| s.parse().ok()).unwrap_or(0),
        duration_ms: opt("--duration").and_then(|s| s.parse().ok()),
        keymap: opt("--keymap").unwrap_or("main").to_owned(),
        env: ENV_DEPS.iter().map(|v| std::env::var(v).unwrap_or_default()).collect(),
    }
}

/// Renders twice through a daemon-mode engine (cold, then warm) and prints
/// per-segment timings.
fn explain(req: Request) {
    let engine = Engine::new(false);
    let since = Instant::now();
    let t = Instant::now();
    let cold = engine.render(&req, Duration::ZERO, since, Shell::Plain);
    let cold_t = t.elapsed();
    // Wait for async work (git status) to land.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut warm = engine.render(&req, Duration::from_millis(50), since, Shell::Plain);
    while warm.pending && Instant::now() < deadline {
        warm = engine.render(&req, Duration::from_millis(50), since, Shell::Plain);
    }
    let mut samples: Vec<(Duration, engine::Rendered)> = (0..500)
        .map(|_| {
            let t = Instant::now();
            let r = engine.render(&req, Duration::ZERO, since, Shell::Plain);
            (t.elapsed(), r)
        })
        .collect();
    samples.sort_by_key(|s| s.0);
    let p99 = samples[samples.len() * 99 / 100].0;
    let (warm_t, warm) = samples.swap_remove(samples.len() / 2);

    println!("frame 1, cold : {:>9.1?}  pending={}", cold_t, cold.pending);
    println!("frame 1, warm : {:>9.1?}  p50 of 500, p99 {:.1?}, pending={}", warm_t, p99, warm.pending);
    for (name, d) in &warm.timings {
        println!("  {name:<10}: {d:>9.1?}");
    }
    if let Some(d) = engine.git_compute_time(std::path::Path::new(&req.cwd)) {
        println!("git status (async, off critical path): {d:.1?}");
    }
    println!("\n{}   {}", warm.left, warm.right);
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
