//! Untracked-file counting with an in-memory equivalent of git's untracked
//! cache: each directory's listing and ignore classification is kept, keyed by
//! the directory's (ino, mtime). Re-runs cost one `stat` per visible directory
//! instead of a `readdir` (~3µs vs ~90µs here). Adding, removing or renaming an
//! entry bumps its parent dir's mtime, which is all untracked detection needs.
//!
//! Semantics match `git status -unormal`: a directory without tracked files is
//! one entry if it holds any untracked file or is a nested repo.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::entry::Mode;

pub type FileKey = Option<(u64, i64, i64, u64)>;

pub fn file_key(p: &Path) -> FileKey {
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
    /// Dirs: untracked count contributed by this child as of `Cache::epoch`.
    /// Stays valid while no change (FS or index) lands at or below it.
    memo: Option<(u64, u32)>,
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
    /// Bumped on every walk that cannot trust memos, retiring all of them.
    epoch: u64,
    pub stats: Stats,
}

impl Cache {
    pub fn is_warm(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// With `trust` (from a watcher), only dirs with changes are revalidated
    /// with `stat` and subtrees without changes reuse their counts.
    pub fn count(&mut self, repo: &gix::Repository, index: &IndexView, trust: Option<&Trust>) -> Option<u32> {
        let workdir = repo.workdir()?.to_path_buf();
        let globals: Vec<FileKey> = global_exclude_files(repo).iter().map(|p| file_key(p)).collect();
        if globals != self.globals {
            self.nodes.clear();
            self.globals = globals;
        }
        self.stats = Stats::default();
        if trust.is_none() {
            self.epoch += 1;
        }
        let mut w = Walk {
            cache: self,
            repo,
            stack: None,
            index,
            workdir,
            trust,
            forced: false,
            count: 0,
        };
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

/// Case-folded (ASCII, like git) lookup tables for an index, built once per
/// index version (~2.5ms for aports' 23k entries) and kept across runs.
#[derive(Default)]
pub struct Icase {
    files: HashMap<BString, usize>,
    /// Every directory holding entries, to the first entry below it.
    dirs: HashMap<BString, usize>,
}

impl Icase {
    pub fn new(state: &gix::index::State) -> Self {
        let mut t = Icase::default();
        for (i, e) in state.entries().iter().enumerate() {
            let p = e.path(state).to_ascii_lowercase();
            let mut d = p.as_slice();
            while let Some(j) = d.rfind_byte(b'/') {
                d = &d[..j];
                if t.dirs.contains_key(d) {
                    break;
                }
                t.dirs.insert(d.into(), i);
            }
            t.files.entry(p.into()).or_insert(i);
        }
        t
    }
}

/// Index lookups that honor `core.ignoreCase` like git does (macOS default).
pub struct IndexView<'a> {
    pub state: &'a gix::index::State,
    icase: Option<&'a Icase>,
}

impl<'a> IndexView<'a> {
    /// `icase`: tables for `state`, when `core.ignoreCase` is set.
    pub fn new(state: &'a gix::index::State, icase: Option<&'a Icase>) -> Self {
        Self { state, icase }
    }

    pub fn entry(&self, path: &BStr) -> Option<&'a gix::index::Entry> {
        let state = self.state;
        state.entry_by_path(path).or_else(|| {
            let i = *self.icase?.files.get(path.to_ascii_lowercase().as_slice())?;
            state.entries().get(i)
        })
    }

    /// Any tracked entry below `dir` (no trailing slash).
    fn has_entries_under(&self, dir: &BStr) -> bool {
        self.dir_entry(dir).is_some()
    }

    /// An entry inside `dir`, or `dir` itself if it is a submodule.
    fn dir_entry(&self, dir: &BStr) -> Option<&'a gix::index::Entry> {
        let state = self.state;
        state.entry_closest_to_directory_or_directory(dir).or_else(|| {
            let icase = self.icase?;
            let key = dir.to_ascii_lowercase();
            let i = icase.dirs.get(key.as_slice()).or_else(|| icase.files.get(key.as_slice()))?;
            let e = state.entries().get(*i)?;
            (icase.dirs.contains_key(key.as_slice()) || e.mode == Mode::COMMIT).then_some(e)
        })
    }

    /// `dir` spelled as in the index, if it holds tracked entries.
    pub fn dir_spelling(&self, dir: &BStr) -> Option<BString> {
        let p = self.dir_entry(dir)?.path(self.state);
        Some(p.get(..dir.len()).unwrap_or(p).into())
    }
}

fn debug_entry(rela: &BStr) {
    if *crate::DEBUG {
        eprintln!("untracked: {rela}");
    }
}

pub(crate) fn parent(p: &BStr) -> &BStr {
    p.rfind_byte(b'/').map_or(b"".as_bstr(), |i| p[..i].as_bstr())
}

/// What changed since the previous walk.
pub struct Trust<'a> {
    /// Changed paths. A changed dir may have been moved in with stale
    /// cached descendants, so its whole subtree is revalidated.
    changes: &'a HashSet<BString>,
    /// Dirs whose listing may have changed: each changed path and its parent.
    restat: HashSet<BString>,
    /// Every ancestor of a changed path or index entry: memos there are stale.
    touched: HashSet<BString>,
}

impl<'a> Trust<'a> {
    /// `changes`: FS paths from a watcher. `index_paths`: index entries added,
    /// removed or changed (they flip files between tracked and untracked).
    pub fn new(changes: &'a HashSet<BString>, index_paths: &[BString]) -> Self {
        let mut t = Trust { changes, restat: HashSet::new(), touched: HashSet::new() };
        for p in changes {
            t.restat.insert(p.clone());
            t.restat.insert(parent(p.as_bstr()).to_owned());
        }
        for p in changes.iter().chain(index_paths) {
            let mut a = p.as_bstr();
            t.touched.insert(a.to_owned());
            while !a.is_empty() {
                a = parent(a);
                if !t.touched.insert(a.to_owned()) {
                    break;
                }
            }
        }
        t
    }
}

struct Walk<'a, 'r, 'i> {
    cache: &'a mut Cache,
    repo: &'r gix::Repository,
    /// Ignore rules, built only when a dir has to be re-read (~0.6ms).
    stack: Option<gix::AttributeStack<'r>>,
    index: &'a IndexView<'i>,
    workdir: PathBuf,
    /// None: revalidate everything.
    trust: Option<&'a Trust<'a>>,
    /// Inside a changed dir: revalidate everything below.
    forced: bool,
    count: u32,
}

impl Walk<'_, '_, '_> {
    /// Visits dir `rela` ("" = root). With `probe`, stops at the first untracked
    /// file and only reports whether one exists, counting nothing.
    fn walk(&mut self, rela: &mut BString, probe: bool) -> bool {
        let forced = self.forced;
        self.forced |= self.trust.is_some_and(|t| t.changes.contains(rela));
        let found = match self.load(rela) {
            Some((children, _)) => self.walk_children(rela, children, probe),
            None => false,
        };
        self.forced = forced;
        found
    }

    /// A child dir's count from the previous walk, if nothing touched it.
    fn memo(&self, rela: &BString, memo: Option<(u64, u32)>) -> Option<u32> {
        let t = self.trust?;
        match memo {
            Some((epoch, n)) if epoch == self.cache.epoch && !self.forced && !t.touched.contains(rela) => Some(n),
            _ => None,
        }
    }

    fn walk_children(&mut self, rela: &mut BString, mut children: Vec<Child>, probe: bool) -> bool {
        let base = rela.len();
        let mut found = false;
        for c in children.iter_mut().filter(|c| !c.ignored) {
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
                Kind::Dir if probe => self.visit_dir(rela, true),
                Kind::Dir => match self.memo(rela, c.memo) {
                    Some(n) => {
                        self.count += n;
                        n > 0
                    }
                    None => {
                        let before = self.count;
                        let hit = self.visit_dir(rela, false);
                        c.memo = Some((self.cache.epoch, self.count - before));
                        hit
                    }
                },
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
        if let Some(t) = self.trust
            && !self.forced
            && !t.restat.contains(rela)
            && let Some(node) = self.cache.nodes.get_mut(rela)
        {
            return Some((std::mem::take(&mut node.children), node.has_dotgit));
        }
        let abs = self.workdir.join(gix::path::from_bstr(rela.as_bstr()).ok()?);
        self.cache.stats.dirs_stat += 1;
        let key = file_key(&abs);
        key?;
        let mut stale_subtree = false;
        if let Some(node) = self.cache.nodes.get_mut(rela)
            && node.key == key
        {
            if node.gitignore.is_none() || file_key(&abs.join(".gitignore")) == node.gitignore {
                return Some((std::mem::take(&mut node.children), node.has_dotgit));
            }
            stale_subtree = true;
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
            let stack = match &mut self.stack {
                Some(s) => s,
                None => self.stack.insert(
                    self.repo
                        .excludes(
                            self.index.state,
                            None,
                            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
                        )
                        .ok()?,
                ),
            };
            let ignored = stack
                .at_entry(path.as_bstr(), Some(mode))
                .map(|p| p.is_excluded())
                .unwrap_or(false);
            children.push(Child { name, kind, ignored, memo: None });
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
