//! Untracked-file counting with an in-memory equivalent of git's untracked
//! cache: each directory's listing and ignore classification is kept, keyed by
//! the directory's (ino, mtime). Re-runs cost one `stat` per visible directory
//! instead of a `readdir` (~3µs vs ~90µs here). Adding, removing or renaming an
//! entry bumps its parent dir's mtime, which is all untracked detection needs.
//!
//! Semantics match `git status -unormal`: a directory without tracked files is
//! one entry if it holds any untracked file or is a nested repo.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::entry::Mode;

type FileKey = Option<(u64, i64, i64, u64)>;

fn file_key(p: &Path) -> FileKey {
    let m = fs::symlink_metadata(p).ok()?;
    Some((m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

struct Child {
    name: BString,
    kind: Kind,
    ignored: bool,
}

struct Node {
    key: FileKey,
    gitignore: FileKey,
    has_dotgit: bool,
    children: Vec<Child>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub dirs_stat: u32,
    pub dirs_read: u32,
}

#[derive(Default)]
pub struct Cache {
    nodes: BTreeMap<BString, Node>,
    globals: Vec<FileKey>,
    pub stats: Stats,
}

impl Cache {
    pub fn is_warm(&self) -> bool {
        !self.nodes.is_empty()
    }

    pub fn count(&mut self, repo: &gix::Repository, index: &gix::index::State) -> Option<u32> {
        let workdir = repo.workdir()?.to_path_buf();
        let globals: Vec<FileKey> = global_exclude_files(repo).iter().map(|p| file_key(p)).collect();
        if globals != self.globals {
            self.nodes.clear();
            self.globals = globals;
        }
        let mut stack = repo
            .excludes(index, None, gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped)
            .ok()?;
        self.stats = Stats::default();
        let ignore_case = repo.config_snapshot().boolean("core.ignoreCase").unwrap_or(false);
        let icase = ignore_case.then(|| index.prepare_icase_backing());
        let index = IndexView { state: index, icase: icase.as_ref() };
        let mut w = Walk { cache: self, stack: &mut stack, index, workdir, count: 0 };
        w.walk(&mut BString::default(), false);
        Some(w.count)
    }
}

fn global_exclude_files(repo: &gix::Repository) -> Vec<PathBuf> {
    let mut v = vec![repo.common_dir().join("info/exclude")];
    let cfg = repo.config_snapshot();
    if let Ok(Some(p)) = cfg.trusted_path("core.excludesFile") {
        v.push(p);
    } else if let Some(base) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
    {
        v.push(base.join("git/ignore"));
    }
    v
}

/// Index lookups that honor `core.ignoreCase` like git does (macOS default).
struct IndexView<'a> {
    state: &'a gix::index::State,
    icase: Option<&'a gix::index::AccelerateLookup<'a>>,
}

impl IndexView<'_> {
    fn entry(&self, path: &BStr) -> Option<&gix::index::Entry> {
        match self.icase {
            Some(l) => self.state.entry_by_path_icase(path, true, l),
            None => self.state.entry_by_path(path),
        }
    }

    /// Any tracked entry below `dir` (no trailing slash).
    fn has_entries_under(&self, dir: &BStr) -> bool {
        match self.icase {
            Some(l) => self.state.entry_closest_to_directory_or_directory_icase(dir, true, l).is_some(),
            None => self.state.entry_closest_to_directory_or_directory(dir).is_some(),
        }
    }
}

fn debug_entry(rela: &BStr) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("ASHELL_DEBUG").is_some());
    if *ON {
        eprintln!("untracked: {rela}");
    }
}

struct Walk<'a, 'r> {
    cache: &'a mut Cache,
    stack: &'a mut gix::AttributeStack<'r>,
    index: IndexView<'a>,
    workdir: PathBuf,
    count: u32,
}

impl Walk<'_, '_> {
    /// Visits dir `rela` ("" = root). With `probe`, stops at the first untracked
    /// file and only reports whether one exists, counting nothing.
    fn walk(&mut self, rela: &mut BString, probe: bool) -> bool {
        match self.load(rela) {
            Some((children, _)) => self.walk_children(rela, children, probe),
            None => false,
        }
    }

    fn walk_children(&mut self, rela: &mut BString, children: Vec<Child>, probe: bool) -> bool {
        let base = rela.len();
        let mut found = false;
        for c in children.iter().filter(|c| !c.ignored) {
            if base > 0 {
                rela.push(b'/');
            }
            rela.extend_from_slice(&c.name);
            let hit = match c.kind {
                Kind::File => {
                    let untracked = self.index.entry(rela.as_bstr()).is_none();
                    if untracked && !probe {
                        self.count += 1;
                        debug_entry(rela.as_bstr());
                    }
                    untracked
                }
                Kind::Dir => self.visit_dir(rela, probe),
            };
            rela.truncate(base);
            if hit {
                found = true;
                if probe {
                    break;
                }
            }
        }
        self.put_back(rela, children);
        found
    }

    fn visit_dir(&mut self, rela: &mut BString, probe: bool) -> bool {
        if self.index.entry(rela.as_bstr()).is_some_and(|e| e.mode == Mode::COMMIT) {
            return false; // submodule: tracked
        }
        if self.index.has_entries_under(rela.as_bstr()) {
            return self.walk(rela, probe);
        }
        let shown = match self.load(rela) {
            Some((children, true)) => {
                self.put_back(rela, children);
                true // nested repo
            }
            Some((children, false)) => self.walk_children(rela, children, true),
            None => false,
        };
        if shown && !probe {
            self.count += 1;
            debug_entry(rela.as_bstr());
        }
        shown
    }

    /// Returns the dir's children (taken out of the cache; give them back with
    /// `put_back`) and whether it contains `.git`.
    fn load(&mut self, rela: &BString) -> Option<(Vec<Child>, bool)> {
        let abs = self.workdir.join(gix::path::from_bstr(rela.as_bstr()).ok()?);
        self.cache.stats.dirs_stat += 1;
        let key = file_key(&abs);
        key?;
        let mut stale_subtree = false;
        if let Some(node) = self.cache.nodes.get_mut(rela) {
            if node.key == key {
                if node.gitignore.is_none() || file_key(&abs.join(".gitignore")) == node.gitignore {
                    return Some((std::mem::take(&mut node.children), node.has_dotgit));
                }
                stale_subtree = true;
            }
        }

        self.cache.stats.dirs_read += 1;
        let mut children = Vec::new();
        let mut has_dotgit = false;
        let mut has_gitignore = false;
        let base = rela.len();
        let mut path = rela.clone();
        for e in fs::read_dir(&abs).ok()?.flatten() {
            let name = gix::path::os_string_into_bstring(e.file_name()).ok()?;
            match name.as_bytes() {
                b".git" => {
                    has_dotgit = true;
                    continue;
                }
                b".gitignore" => has_gitignore = true,
                _ => {}
            }
            let kind = match e.file_type() {
                Ok(t) if t.is_dir() => Kind::Dir,
                _ => Kind::File,
            };
            path.truncate(base);
            if base > 0 {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            let mode = if kind == Kind::Dir { Mode::DIR } else { Mode::FILE };
            let ignored = self
                .stack
                .at_entry(path.as_bstr(), Some(mode))
                .map(|p| p.is_excluded())
                .unwrap_or(false);
            children.push(Child { name, kind, ignored });
        }
        let gitignore = if has_gitignore { file_key(&abs.join(".gitignore")) } else { None };
        if let Some(old) = self.cache.nodes.get(rela) {
            stale_subtree |= old.gitignore != gitignore;
        }
        if stale_subtree {
            self.drop_subtree(rela.as_bstr());
        }
        self.cache
            .nodes
            .insert(rela.clone(), Node { key, gitignore, has_dotgit, children: Vec::new() });
        Some((children, has_dotgit))
    }

    fn put_back(&mut self, rela: &BString, children: Vec<Child>) {
        if let Some(n) = self.cache.nodes.get_mut(rela) {
            n.children = children;
        }
    }

    /// Ignore rules under `rela` changed: forget every cached descendant.
    fn drop_subtree(&mut self, rela: &BStr) {
        let mut prefix = rela.to_owned();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let doomed: Vec<BString> = self
            .cache
            .nodes
            .range::<BString, _>(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for k in doomed {
            self.cache.nodes.remove(&k);
        }
    }
}
