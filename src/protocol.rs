//! Wire format: records end with RS (0x1E), fields split by US (0x1F).
//!
//! client -> daemon
//!   H US <proto> US <shell>                                   handshake
//!   P US <seq> US <cwd> US <cols> US <status> US <dur_ms> US <keymap> [US <env value>]*
//!   Q                                                          shut down daemon
//! daemon -> client
//!   H US <proto> [US <env var name>]*                          env deps, in request order
//!   F US <seq> US <final 0|1> US <left> US <right>             prompt frame

use crate::render::Shell;

pub const US: u8 = 0x1f;
pub const RS: u8 = 0x1e;
pub const PROTO: &str = "1";

/// Env vars segments read. Clients send their values with every request.
pub const ENV_DEPS: &[&str] = &["HOME"];

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub seq: u64,
    pub cwd: String,
    #[allow(dead_code)] // width-based segment dropping, M2
    pub cols: u16,
    pub status: i32,
    /// `None` when no command ran (empty enter, first prompt).
    pub duration_ms: Option<u64>,
    pub keymap: String,
    /// Values for `ENV_DEPS`, same order.
    pub env: Vec<String>,
}

impl Request {
    pub fn env(&self, name: &str) -> Option<&str> {
        let i = ENV_DEPS.iter().position(|v| *v == name)?;
        self.env.get(i).map(String::as_str).filter(|s| !s.is_empty())
    }

    pub fn ran_command(&self) -> bool {
        self.duration_ms.is_some()
    }
}

pub enum Msg {
    Hello { shell: Shell },
    Prompt(Request),
    Quit,
}

pub fn parse(rec: &[u8]) -> Option<Msg> {
    let s = std::str::from_utf8(rec).ok()?;
    let mut f = s.split(US as char);
    match f.next()? {
        "H" => {
            let _proto = f.next()?;
            let shell = Shell::parse(f.next().unwrap_or("plain"))?;
            Some(Msg::Hello { shell })
        }
        "P" => {
            let seq = f.next()?.parse().ok()?;
            let cwd = f.next()?.to_owned();
            let cols = f.next()?.parse().unwrap_or(80);
            let status = f.next()?.parse().unwrap_or(0);
            let duration_ms = f.next()?.parse().ok();
            let keymap = f.next().unwrap_or("").to_owned();
            let env = f.map(str::to_owned).collect();
            Some(Msg::Prompt(Request { seq, cwd, cols, status, duration_ms, keymap, env }))
        }
        "Q" => Some(Msg::Quit),
        _ => None,
    }
}

pub fn hello_reply(out: &mut Vec<u8>) {
    out.clear();
    out.push(b'H');
    out.push(US);
    out.extend_from_slice(PROTO.as_bytes());
    for v in ENV_DEPS {
        out.push(US);
        out.extend_from_slice(v.as_bytes());
    }
    out.push(RS);
}

pub fn frame(out: &mut Vec<u8>, seq: u64, fin: bool, left: &str, right: &str) {
    use std::io::Write;
    out.clear();
    let _ = write!(out, "F\x1f{seq}\x1f{}\x1f", fin as u8);
    out.extend_from_slice(left.as_bytes());
    out.push(US);
    out.extend_from_slice(right.as_bytes());
    out.push(RS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prompt_request() {
        let Some(Msg::Prompt(r)) = parse(b"P\x1f7\x1f/tmp\x1f120\x1f1\x1f\x1fmain\x1f/Users/x") else {
            panic!()
        };
        assert_eq!((r.seq, r.cwd.as_str(), r.cols, r.status), (7, "/tmp", 120, 1));
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.env("HOME"), Some("/Users/x"));
    }

    #[test]
    fn frame_layout() {
        let mut b = Vec::new();
        frame(&mut b, 3, true, "L", "R");
        assert_eq!(b, b"F\x1f3\x1f1\x1fL\x1fR\x1e");
    }
}
