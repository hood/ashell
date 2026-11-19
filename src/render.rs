use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Zsh,
    Bash,
    Plain,
}

impl Shell {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "plain" => Some(Self::Plain),
            _ => None,
        }
    }
}

/// SGR parameter strings, e.g. "1;36".
pub mod sgr {
    pub const DIR: &str = "1;36";
    pub const BRANCH: &str = "35";
    pub const STAGED: &str = "32";
    pub const MODIFIED: &str = "33";
    pub const UNTRACKED: &str = "34";
    pub const CONFLICT: &str = "31";
    pub const SYNC: &str = "36";
    pub const STASH: &str = "37";
    pub const DIM: &str = "2";
    pub const OK: &str = "1;32";
    pub const ERR: &str = "1;31";
    pub const DURATION: &str = "33";
}

/// Builds one prompt line with shell-specific zero-width escapes.
///
/// zsh: content lands in a variable expanded under `prompt_subst`, so only `%`
/// needs escaping and `%{ %}` marks escapes. bash: `\x01 \x02` work from
/// variable content, and promptvars never re-expands it.
pub struct Line {
    shell: Shell,
    pub buf: String,
    empty: bool,
}

impl Line {
    pub fn new(shell: Shell) -> Self {
        Self { shell, buf: String::with_capacity(256), empty: true }
    }

    /// Styled segment, space-separated from the previous one.
    pub fn seg(&mut self, sgr: &str, text: &str) {
        if !self.empty {
            self.buf.push(' ');
        }
        self.styled(sgr, text);
    }

    /// Styled text glued to whatever came before.
    pub fn styled(&mut self, sgr: &str, text: &str) {
        self.empty = false;
        self.esc(sgr);
        self.text(text);
        self.esc("0");
    }

    pub fn raw(&mut self, text: &str) {
        self.text(text);
    }

    fn esc(&mut self, sgr: &str) {
        let (open, close) = match self.shell {
            Shell::Zsh => ("%{", "%}"),
            Shell::Bash => ("\x01", "\x02"),
            Shell::Plain => ("", ""),
        };
        let _ = write!(self.buf, "{open}\x1b[{sgr}m{close}");
    }

    fn text(&mut self, t: &str) {
        for c in t.chars() {
            match c {
                '\x1e' | '\x1f' | '\x01' | '\x02' => {}
                '%' if self.shell == Shell::Zsh => self.buf.push_str("%%"),
                c if c.is_control() => self.buf.push('?'),
                c => self.buf.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_escapes_percent_and_strips_separators() {
        let mut l = Line::new(Shell::Zsh);
        l.seg("1", "100%\x1fx");
        assert_eq!(l.buf, "%{\x1b[1m%}100%%x%{\x1b[0m%}");
    }

    #[test]
    fn bash_uses_rl_markers() {
        let mut l = Line::new(Shell::Bash);
        l.seg("2", "$(x)");
        assert_eq!(l.buf, "\x01\x1b[2m\x02$(x)\x01\x1b[0m\x02");
    }
}
