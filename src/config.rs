//! User config: `$ASHELL_CONFIG`, else `$XDG_CONFIG_HOME/ashell/config.toml`,
//! else `~/.config/ashell/config.toml`. Read once when the daemon starts
//! (`ashell stop` applies changes). Every key is optional; unknown keys are
//! errors so typos surface. A broken file falls back to defaults and the
//! prompt shows a warning segment.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::PathBuf;

use serde::Deserialize;

use crate::toolchain::TOOLS;

/// Documented defaults, printed by `ashell config default`. A test keeps it
/// equal to `Config::default()`.
pub const DEFAULT_TOML: &str = r##"# ashell config. Every key is optional: delete what you don't change.
# Styles: words among bold dim italic underline blink inverted hidden
# strikethrough, colors (black red green yellow blue purple cyan white,
# bright-<color>, 0-255, #rrggbb), "fg:"/"bg:" prefixes, or "none".

# Segment order. Leave a segment out to disable it.
left = ["directory", "git", "toolchains", "character"]
right = ["duration", "status"]

[directory]
truncate = 4               # path components kept (0: all)
truncation_symbol = "…/"
home_symbol = "~"
repo_relative = true       # inside a repo: <repo name>/<path in repo>
style = "bold cyan"

[git]
branch_symbol = "\ue0a0 "       # needs a Nerd Font / powerline glyph
detached_prefix = "@"
branch_style = "purple"
pending_symbol = "…"       # status not computed yet
stale_style = "dim"        # status being recomputed
conflicted = "="
conflicted_style = "red"
staged = "+"
staged_style = "green"
modified = "!"
modified_style = "yellow"
untracked = "?"
untracked_style = "blue"
ahead = "⇡"
behind = "⇣"
sync_style = "cyan"
stash = "*"
stash_style = "white"

[toolchains]
version_prefix = "v"
# Per tool: rust node python go ruby java bun deno.
# [toolchains.java]
# disabled = true
# symbol = "J "
# style = "bold red"

[character]
success_symbol = "❯"
error_symbol = "❯"
vicmd_symbol = "❮"
success_style = "bold green"
error_style = "bold red"

[duration]
min_ms = 2000              # show commands that took at least this long
style = "yellow"

[status]
style = "bold red"
"##;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Segment {
    Directory,
    Git,
    Toolchains,
    Character,
    Duration,
    Status,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub left: Vec<Segment>,
    pub right: Vec<Segment>,
    pub directory: Directory,
    pub git: Git,
    pub toolchains: Toolchains,
    pub character: Character,
    pub duration: Duration,
    pub status: Status,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Directory {
    pub truncate: usize,
    pub truncation_symbol: String,
    pub home_symbol: String,
    pub repo_relative: bool,
    pub style: Style,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Git {
    pub branch_symbol: String,
    pub detached_prefix: String,
    pub branch_style: Style,
    pub pending_symbol: String,
    pub stale_style: Style,
    pub conflicted: String,
    pub conflicted_style: Style,
    pub staged: String,
    pub staged_style: Style,
    pub modified: String,
    pub modified_style: Style,
    pub untracked: String,
    pub untracked_style: Style,
    pub ahead: String,
    pub behind: String,
    pub sync_style: Style,
    pub stash: String,
    pub stash_style: Style,
}

/// No `deny_unknown_fields`: serde can't combine it with `flatten`. Unknown
/// keys land in `tools` and fail the tool-name check in `Config::parse`.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct Toolchains {
    pub version_prefix: String,
    #[serde(flatten)]
    pub tools: BTreeMap<String, Tool>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Tool {
    pub disabled: bool,
    pub symbol: Option<String>,
    pub style: Option<Style>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Character {
    pub success_symbol: String,
    pub error_symbol: String,
    pub vicmd_symbol: String,
    pub success_style: Style,
    pub error_style: Style,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Duration {
    pub min_ms: u64,
    pub style: Style,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Status {
    pub style: Style,
}

fn s(v: &str) -> String {
    v.to_owned()
}

fn st(v: &str) -> Style {
    Style::parse(v).expect("built-in style")
}

impl Default for Config {
    fn default() -> Self {
        use Segment::*;
        Self {
            left: vec![Directory, Git, Toolchains, Character],
            right: vec![Duration, Status],
            directory: Default::default(),
            git: Default::default(),
            toolchains: Default::default(),
            character: Default::default(),
            duration: Default::default(),
            status: Default::default(),
        }
    }
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            truncate: 4,
            truncation_symbol: s("…/"),
            home_symbol: s("~"),
            repo_relative: true,
            style: st("bold cyan"),
        }
    }
}

impl Default for Git {
    fn default() -> Self {
        Self {
            branch_symbol: s("\u{e0a0} "),
            detached_prefix: s("@"),
            branch_style: st("purple"),
            pending_symbol: s("…"),
            stale_style: st("dim"),
            conflicted: s("="),
            conflicted_style: st("red"),
            staged: s("+"),
            staged_style: st("green"),
            modified: s("!"),
            modified_style: st("yellow"),
            untracked: s("?"),
            untracked_style: st("blue"),
            ahead: s("⇡"),
            behind: s("⇣"),
            sync_style: st("cyan"),
            stash: s("*"),
            stash_style: st("white"),
        }
    }
}

impl Default for Toolchains {
    fn default() -> Self {
        Self { version_prefix: s("v"), tools: BTreeMap::new() }
    }
}

impl Default for Character {
    fn default() -> Self {
        Self {
            success_symbol: s("❯"),
            error_symbol: s("❯"),
            vicmd_symbol: s("❮"),
            success_style: st("bold green"),
            error_style: st("bold red"),
        }
    }
}

impl Default for Duration {
    fn default() -> Self {
        Self { min_ms: 2000, style: st("yellow") }
    }
}

impl Default for Status {
    fn default() -> Self {
        Self { style: st("bold red") }
    }
}

impl Toolchains {
    pub fn tool(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn enabled(&self, name: &str) -> bool {
        self.tool(name).is_none_or(|t| !t.disabled)
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("ASHELL_CONFIG").filter(|p| !p.is_empty()) {
            return Some(p.into());
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("ashell/config.toml"))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let cfg: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        for name in cfg.toolchains.tools.keys() {
            if !TOOLS.iter().any(|t| t.name == name) {
                let known: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
                return Err(format!("unknown toolchain `{name}` (known: {})", known.join(", ")));
            }
        }
        Ok(cfg)
    }

    /// The config, or defaults plus the reason when the file is broken. A
    /// missing file is not an error.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else { return (Self::default(), None) };
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
            Ok(text) => match Self::parse(&text) {
                Ok(c) => (c, None),
                Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
            },
        }
    }
}

/// An SGR parameter string ("1;36"), parsed from Starship-like words.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Style(pub String);

impl Style {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = String::new();
        let mut push = |code: &str| {
            if !out.is_empty() {
                out.push(';');
            }
            out.push_str(code);
        };
        for word in spec.split_whitespace() {
            let w = word.to_ascii_lowercase();
            let code = match w.as_str() {
                "none" => continue,
                "bold" => "1".into(),
                "dim" => "2".into(),
                "italic" => "3".into(),
                "underline" => "4".into(),
                "blink" => "5".into(),
                "inverted" => "7".into(),
                "hidden" => "8".into(),
                "strikethrough" => "9".into(),
                _ => {
                    let (bg, color) = match w.split_once(':') {
                        Some(("fg", c)) => (false, c),
                        Some(("bg", c)) => (true, c),
                        Some(_) => return Err(format!("bad style word `{word}`")),
                        None => (false, w.as_str()),
                    };
                    color_code(color, bg).ok_or_else(|| format!("unknown color `{word}`"))?
                }
            };
            push(&code);
        }
        Ok(Style(out))
    }
}

fn color_code(c: &str, bg: bool) -> Option<String> {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "purple", "cyan", "white"];
    let base = if bg { 40 } else { 30 };
    let ext = if bg { 48 } else { 38 };
    let name = |n: &str| NAMES.iter().position(|x| *x == n).or_else(|| (n == "magenta").then_some(5));
    if let Some(i) = name(c) {
        return Some((base + i).to_string());
    }
    if let Some(i) = c.strip_prefix("bright-").and_then(name) {
        return Some((base + 60 + i).to_string());
    }
    if let Ok(n) = c.parse::<u8>() {
        return Some(format!("{ext};5;{n}"));
    }
    let hex = c.strip_prefix('#').filter(|h| h.len() == 6)?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    let mut s = String::new();
    let _ = write!(s, "{ext};2;{};{};{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
    Some(s)
}

impl<'de> Deserialize<'de> for Style {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(d)?;
        Style::parse(&spec).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_matches_defaults() {
        assert_eq!(Config::parse(DEFAULT_TOML).unwrap(), Config::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let c = Config::parse("right = []\n[git]\nstaged = \"S\"\n[toolchains.java]\ndisabled = true\n").unwrap();
        assert!(c.right.is_empty());
        assert_eq!(c.git.staged, "S");
        assert_eq!(c.git.modified, "!");
        assert_eq!(c.git.staged_style, Style("32".into()));
        assert!(!c.toolchains.enabled("java"));
        assert!(c.toolchains.enabled("rust"));
        assert_eq!(c.left, Config::default().left);
    }

    #[test]
    fn errors_name_the_problem() {
        let e = Config::parse("[git]\nstagd = \"+\"\n").unwrap_err();
        assert!(e.contains("stagd"), "{e}");
        let e = Config::parse("left = [\"dir\"]\n").unwrap_err();
        assert!(e.contains("dir"), "{e}");
        let e = Config::parse("[character]\nsuccess_style = \"bold pink\"\n").unwrap_err();
        assert!(e.contains("pink"), "{e}");
        let e = Config::parse("[toolchains.cobol]\ndisabled = true\n").unwrap_err();
        assert!(e.contains("cobol"), "{e}");
    }

    #[test]
    fn styles() {
        let p = |s: &str| Style::parse(s).unwrap().0;
        assert_eq!(p("bold cyan"), "1;36");
        assert_eq!(p("bright-red"), "91");
        assert_eq!(p("bg:blue fg:208"), "44;38;5;208");
        assert_eq!(p("#ff8800"), "38;2;255;136;0");
        assert_eq!(p("bg:#000001 italic"), "48;2;0;0;1;3");
        assert_eq!(p("none"), "");
        assert_eq!(p("magenta"), "35");
        assert!(Style::parse("fg:#12").is_err());
        assert!(Style::parse("xx:red").is_err());
    }
}
