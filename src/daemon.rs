use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bus::BUS;
use crate::engine::Engine;
use crate::protocol::{self, Msg, RS, Request};
use crate::render::Shell;

/// How long a segment may block frame 1.
pub const DEADLINE: Duration = Duration::from_millis(3);
/// After frame 1, keep listening this long for late FS events from the command that just ran.
const GRACE: Duration = Duration::from_millis(250);
/// Give up on pending segments after this.
const HARD_LIMIT: Duration = Duration::from_secs(10);

pub fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("ASHELL_SOCK") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".into());
    let uid = unsafe { libc::getuid() };
    Path::new(&base).join(format!("ashell-{uid}")).join("ashelld.sock")
}

pub fn run(sock: &Path) -> io::Result<()> {
    let dir = sock.parent().ok_or_else(|| io::Error::other("bad socket path"))?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    let lock = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join("ashelld.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(()); // another daemon owns this socket
    }
    let _ = fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    fs::set_permissions(sock, fs::Permissions::from_mode(0o600))?;

    exit_on_upgrade(sock.to_path_buf());
    let engine = Arc::new(Engine::new(false));
    let me = unsafe { libc::getuid() };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if peer_uid(&stream) != Some(me) {
            continue;
        }
        let engine = engine.clone();
        let sock = sock.to_path_buf();
        std::thread::spawn(move || {
            let _ = serve(stream, &engine, &sock);
        });
    }
    drop(lock); // held for the daemon's lifetime
    Ok(())
}

struct Session {
    seq: AtomicU64,
    shell: Mutex<Shell>,
    out: Mutex<UnixStream>,
}

impl Session {
    fn send_frame(&self, seq: u64, fin: bool, left: &str, right: &str) -> io::Result<()> {
        let mut buf = Vec::with_capacity(left.len() + right.len() + 24);
        protocol::frame(&mut buf, seq, fin, left, right);
        self.out.lock().unwrap().write_all(&buf)
    }
}

fn serve(stream: UnixStream, engine: &Arc<Engine>, sock: &Path) -> io::Result<()> {
    let session = Arc::new(Session {
        seq: AtomicU64::new(0),
        shell: Mutex::new(Shell::Plain),
        out: Mutex::new(stream.try_clone()?),
    });
    let mut rd = BufReader::new(stream);
    let mut rec = Vec::with_capacity(512);
    let mut buf = Vec::with_capacity(64);
    loop {
        rec.clear();
        if rd.read_until(RS, &mut rec)? == 0 {
            return Ok(());
        }
        if rec.last() == Some(&RS) {
            rec.pop();
        }
        match protocol::parse(&rec) {
            Some(Msg::Hello { shell }) => {
                *session.shell.lock().unwrap() = shell;
                protocol::hello_reply(&mut buf);
                session.out.lock().unwrap().write_all(&buf)?;
            }
            Some(Msg::Prompt(req)) => {
                let t0 = Instant::now();
                session.seq.store(req.seq, Ordering::Release);
                let shell = *session.shell.lock().unwrap();
                let r = engine.render(&req, DEADLINE, t0, shell);
                if *crate::DEBUG {
                    eprintln!("frame1 seq={} +{:.1?} pending={}", req.seq, t0.elapsed(), r.pending);
                }
                session.send_frame(req.seq, !r.pending, &r.left, &r.right)?;
                if r.pending || (r.watchable && req.ran_command()) {
                    let (s, e) = (session.clone(), engine.clone());
                    std::thread::spawn(move || follow_up(s, e, req, shell, t0, r.pending, r.left, r.right));
                }
            }
            Some(Msg::Quit) => {
                let _ = fs::remove_file(sock);
                std::process::exit(0);
            }
            None => {}
        }
    }
}

/// Re-renders on every bus event until nothing is pending and the grace window
/// is over, pushing a frame whenever the output changes.
#[allow(clippy::too_many_arguments)]
fn follow_up(
    s: Arc<Session>,
    engine: Arc<Engine>,
    req: Request,
    shell: Shell,
    t0: Instant,
    mut pending: bool,
    mut left: String,
    mut right: String,
) {
    let mut seen = BUS.current();
    loop {
        let end = t0 + if pending { HARD_LIMIT } else { GRACE };
        let now = Instant::now();
        if now >= end {
            return;
        }
        seen = BUS.wait_past(seen, end - now);
        if s.seq.load(Ordering::Acquire) != req.seq {
            return; // a newer prompt took over
        }
        let r = engine.render(&req, Duration::ZERO, t0, shell);
        pending = r.pending;
        if *crate::DEBUG {
            eprintln!("follow-up +{:.1?} changed={}", t0.elapsed(), r.left != left || r.right != right);
        }
        if r.left != left || r.right != right {
            if s.send_frame(req.seq, !pending, &r.left, &r.right).is_err() {
                return;
            }
            (left, right) = (r.left, r.right);
        }
    }
}

/// Exits when our binary is replaced (rebuild, upgrade). Clients see the
/// hangup, fall back for one prompt and spawn the new binary.
fn exit_on_upgrade(sock: PathBuf) {
    use std::os::unix::fs::MetadataExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let key = |p: &Path| fs::metadata(p).ok().map(|m| (m.ino(), m.mtime(), m.mtime_nsec(), m.size()));
    let original = key(&exe);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if key(&exe) != original {
                let _ = fs::remove_file(&sock);
                std::process::exit(0);
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    let rc = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

/// Starts the daemon in a new session with stdio detached, returns immediately.
pub fn spawn_detached(sock: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("--socket").arg(sock);
    // ASHELL_LOG=<file> keeps the detached daemon's stderr (debugging).
    let log = std::env::var_os("ASHELL_LOG").and_then(|p| fs::OpenOptions::new().create(true).append(true).open(p).ok());
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log.map_or_else(Stdio::null, Stdio::from));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(drop)
}

pub fn stop(sock: &Path) -> io::Result<()> {
    let mut s = UnixStream::connect(sock)?;
    s.write_all(b"Q\x1e")
}
