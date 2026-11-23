use std::fmt::Write;

use crate::config::{Color, Style};

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

/// Builds one prompt side with shell-specific zero-width escapes.
///
/// zsh: content lands in a variable expanded under `prompt_subst`, so only `%`
/// needs escaping and `%{ %}` marks escapes. bash: `\x01 \x02` work from
/// variable content, and promptvars never re-expands it.
///
/// Segments are opened with `begin`; their first piece is preceded by the
/// separator (colored from the previous segment's background to this one's)
/// and padding. A segment that prints nothing leaves no trace.
pub struct Line<'a> {
    shell: Shell,
    pub buf: String,
    sep: &'a str,
    pad: &'a str,
    /// Nothing printed on the current visual line yet.
    empty: bool,
    /// `begin` was called and the segment hasn't printed yet.
    starting: bool,
    /// A segment printed and its closing padding is due.
    open: bool,
    /// Background of the last segment printed on this line.
    bg: Option<Color>,
}

impl<'a> Line<'a> {
    pub fn new(shell: Shell, sep: &'a str, pad: &'a str) -> Self {
        Self { shell, buf: String::with_capacity(256), sep, pad, empty: true, starting: false, open: false, bg: None }
    }

    /// Starts a new segment.
    pub fn begin(&mut self) {
        self.close();
        self.starting = true;
    }

    /// Styled piece, space-separated from the previous one in the segment.
    pub fn seg(&mut self, style: &Style, text: &str) {
        self.seg_raw(&style.sgr, style.bg, text);
    }

    pub fn seg_raw(&mut self, sgr: &str, bg: Option<Color>, text: &str) {
        if self.starting {
            self.enter(bg);
        } else if !self.empty {
            self.fill(" ");
        }
        self.put(sgr, bg, text);
    }

    /// Styled piece glued to whatever came before.
    pub fn styled(&mut self, style: &Style, text: &str) {
        if self.starting {
            self.enter(style.bg);
        }
        self.put(&style.sgr, style.bg, text);
    }

    /// Unstyled text after the line's last segment.
    pub fn raw(&mut self, text: &str) {
        self.text(text);
    }

    /// Ends the visual line (closing an open block) and starts the next.
    pub fn newline(&mut self) {
        self.end();
        self.buf.push('\n');
        self.empty = true;
        self.starting = false;
    }

    /// Closes the last segment and, when it has a background, caps it with
    /// the separator.
    pub fn end(&mut self) {
        self.close();
        if let Some(bg) = self.bg.take() {
            self.transition(Some(bg), None);
        }
    }

    fn enter(&mut self, bg: Option<Color>) {
        self.starting = false;
        if !self.empty {
            self.transition(self.bg, bg);
        }
        self.empty = false;
        self.open = true;
        self.bg = bg;
        self.pad_block();
    }

    fn close(&mut self) {
        if self.open {
            self.open = false;
            self.pad_block();
        }
    }

    /// Padding only makes sense inside a colored block.
    fn pad_block(&mut self) {
        if self.bg.is_some() {
            self.fill(self.pad);
        }
    }

    /// The separator, from the `from` background into `to`.
    fn transition(&mut self, from: Option<Color>, to: Option<Color>) {
        let mut sgr = String::new();
        if let Some(c) = from {
            c.write(false, &mut sgr);
        }
        if let Some(c) = to {
            if !sgr.is_empty() {
                sgr.push(';');
            }
            c.write(true, &mut sgr);
        }
        self.paint(&sgr, self.sep);
    }

    /// Text on the current segment's background.
    fn fill(&mut self, text: &str) {
        match self.bg {
            Some(c) => {
                let mut sgr = String::new();
                c.write(true, &mut sgr);
                self.paint(&sgr, text);
            }
            None => self.paint("", text),
        }
    }

    /// A piece; without its own background it inherits the segment's.
    fn put(&mut self, sgr: &str, bg: Option<Color>, text: &str) {
        self.empty = false;
        match (bg, self.bg) {
            (None, Some(c)) => {
                let mut full = String::from(sgr);
                if !full.is_empty() {
                    full.push(';');
                }
                c.write(true, &mut full);
                self.paint(&full, text);
            }
            _ => self.paint(sgr, text),
        }
    }

    fn paint(&mut self, sgr: &str, text: &str) {
        if sgr.is_empty() || text.is_empty() {
            self.text(text); // style "none"
            return;
        }
        self.esc(sgr);
        self.text(text);
        self.esc("0");
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

    fn st(s: &str) -> Style {
        Style::parse(s).unwrap()
    }

    #[test]
    fn zsh_escapes_percent_and_strips_separators() {
        let mut l = Line::new(Shell::Zsh, " ", "");
        l.begin();
        l.seg(&st("bold"), "100%\x1fx");
        assert_eq!(l.buf, "%{\x1b[1m%}100%%x%{\x1b[0m%}");
    }

    #[test]
    fn bash_uses_rl_markers() {
        let mut l = Line::new(Shell::Bash, " ", "");
        l.begin();
        l.seg(&st("dim"), "$(x)");
        assert_eq!(l.buf, "\x01\x1b[2m\x02$(x)\x01\x1b[0m\x02");
    }

    #[test]
    fn plain_segments_join_with_spaces() {
        let mut l = Line::new(Shell::Plain, " ", "");
        for (style, text) in [("red", "a"), ("none", ""), ("blue", "b")] {
            l.begin();
            if !text.is_empty() {
                l.seg(&st(style), text);
            }
        }
        l.begin();
        l.seg(&st("none"), "c");
        l.styled(&st("green"), "d");
        l.end();
        assert_eq!(l.buf, "\x1b[31ma\x1b[0m \x1b[34mb\x1b[0m c\x1b[32md\x1b[0m");
    }

    #[test]
    fn blocks_join_with_colored_separators() {
        let mut l = Line::new(Shell::Plain, ">", " ");
        l.begin();
        l.seg(&st("bg:red"), "a");
        l.begin();
        l.seg(&st("fg:black bg:blue"), "b");
        l.seg(&st("bold"), "c"); // inherits the block's background
        l.begin(); // prints nothing: no separator
        l.newline();
        l.begin();
        l.seg(&st("green"), ">");
        l.end();
        let e = "\x1b[41m \x1b[0m\x1b[41ma\x1b[0m\x1b[41m \x1b[0m\
                 \x1b[31;44m>\x1b[0m\x1b[44m \x1b[0m\x1b[30;44mb\x1b[0m\x1b[44m \x1b[0m\x1b[1;44mc\x1b[0m\x1b[44m \x1b[0m\
                 \x1b[34m>\x1b[0m\n\
                 \x1b[32m>\x1b[0m";
        assert_eq!(l.buf, e);
    }
}
