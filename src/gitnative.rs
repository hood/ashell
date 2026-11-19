//! In-process git status via gitoxide, incremental when the caller knows
//! which paths changed (FS watcher). Counts mirror `git status --porcelain=v2`.
//!
//! Three parts, each reused when its inputs are unchanged:
//! - staged (HEAD tree vs index): free when the index cache-tree matches HEAD;
//!   else the tree flattened into an index is cached per tree id (building
//!   it is ~35ms in aports, diffing two indexes ~1ms).
//! - modified/conflicted (index vs worktree): only paths the watcher saw
//!   change, plus index entries that changed since last time, are re-checked.
//! - untracked: `untracked::Cache`, skipping `stat` on untouched dirs and
//!   reusing per-subtree counts.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::entry::Mode;
use gix::status::plumbing::index_as_worktree::{self as iaw, EntryStatus};

use crate::git::Status;
use crate::untracked::{self, FileKey, IndexView, file_key};

/// Paths (relative to the workdir) changed since the previous call.
pub enum Changes {
    Unknown,
    Known(HashSet<BString>),
}

/// Above this many touched tracked paths a full index/worktree pass is cheaper
/// than matching pathspecs.
const MAX_PATHSPECS: usize = 256;

#[derive(Default)]
pub struct State {
    /// Reopened when the config changes.
    repo: Option<(std::path::PathBuf, FileKey, gix::Repository)>,
    refs: Option<Refs>,
    /// HEAD tree as an index, for the staged diff.
    tree: Option<(gix::ObjectId, gix::index::State)>,
    staged: Option<((FileKey, gix::ObjectId), Vec<BString>)>,
    worktree: Option<Worktree>,
    /// Case-folded lookups for the index version held alongside.
    icase: Option<(gix::worktree::Index, untracked::Icase)>,
    pub untracked: untracked::Cache,
    /// Set by the caller: the state reflects the worktree as of the last call
    /// and every change since is reported via `Changes::Known`.
    pub synced: bool,
    pub stats: Stats,
}

struct Worktree {
    /// Index the sets were computed against.
    index: gix::worktree::Index,
    /// Per-index setup for checks (~1ms in aports, rebuilt when it changes).
    ctx: WorktreeCtx,
    modified: BTreeSet<BString>,
    conflicted: BTreeSet<BString>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub staged_reused: bool,
    pub tree_reused: bool,
    pub cache_tree: bool,
    /// None: full pass; Some(n): n pathspecs re-checked.
    pub worktree_checked: Option<usize>,
}

pub fn status(workdir: &Path, state: &mut State, changes: Changes) -> Option<Status> {
    let r = status_inner(workdir, state, changes);
    if r.is_none() {
        *state = State::default();
    }
    r
}

fn status_inner(workdir: &Path, state: &mut State, changes: Changes) -> Option<Status> {
    let t0 = std::time::Instant::now();
    let lap = |what: &str| {
        if *crate::DEBUG {
            eprintln!("  {what}: {:?}", t0.elapsed());
        }
    };
    let changes = match changes {
        Changes::Known(c) if state.synced => Some(c),
        _ => None,
    };
    state.stats = Stats::default();

    if state.repo.as_ref().is_none_or(|(p, k, _)| k.is_none() || file_key(p) != *k) {
        // The index is written by atomic rename, so its trailing checksum
        // only guards against disk corruption: skipping it saves ~2.7ms per
        // read of aports' 3.4MB index.
        let opts = gix::open::Options::default().config_overrides(["index.skipHash=true"]);
        let repo = gix::open_opts(workdir, opts).ok()?;
        let config = repo.common_dir().join("config");
        state.repo = Some((config.clone(), file_key(&config), repo));
    }
    let repo = &state.repo.as_ref()?.2;
    lap("open");
    let index_key = file_key(&repo.index_path());
    let index = repo.index_or_empty().ok()?;
    lap("index");

    let (tree_id, ahead_behind) = refs(repo, &mut state.refs)?;
    match &state.staged {
        Some((k, _)) if *k == (index_key, tree_id) && index_key.is_some() => state.stats.staged_reused = true,
        _ => {
            let rewrites = rewrites(repo)?;
            let staged = match staged_via_cache_tree(repo, &index, tree_id, rewrites.as_ref()) {
                Some(v) => {
                    state.stats.cache_tree = true;
                    #[cfg(test)]
                    {
                        let t: gix::index::State = repo.index_from_tree(&tree_id).ok()?.into();
                        let mut full = staged(repo, &t, &index, rewrites)?;
                        let mut fast = v.clone();
                        full.sort();
                        fast.sort();
                        assert_eq!(fast, full, "cache-tree staged diff disagrees with full diff");
                        CACHE_TREE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    v
                }
                None => {
                    if state.tree.as_ref().is_some_and(|(id, _)| *id == tree_id) {
                        state.stats.tree_reused = true;
                    } else {
                        let t: gix::index::State = repo.index_from_tree(&tree_id).ok()?.into();
                        state.tree = Some((tree_id, t));
                    }
                    staged(repo, &state.tree.as_ref()?.1, &index, rewrites)?
                }
            };
            state.staged = Some(((index_key, tree_id), staged));
        }
    }
    lap("tree-index");

    let ignore_case = repo.config_snapshot().boolean("core.ignoreCase").unwrap_or(false);
    if !ignore_case {
        state.icase = None;
    } else if state.icase.as_ref().is_none_or(|(i, _)| !std::sync::Arc::ptr_eq(i, &index)) {
        state.icase = Some((index.clone(), untracked::Icase::new(&index)));
    }
    let view = IndexView::new(&index, state.icase.as_ref().map(|(_, t)| t));
    lap("icase");

    // Index entries that changed since the sets were computed: their status
    // may differ even if the file did not change.
    let index_touched = match &state.worktree {
        Some(w) if std::sync::Arc::ptr_eq(&w.index, &index) => Some(Vec::new()),
        Some(w) => index_changes(&w.index, &index, MAX_PATHSPECS),
        None => None,
    };
    let specs = match (&changes, &index_touched) {
        (Some(c), Some(touched)) => tracked_specs(&view, c).map(|mut s| {
            s.extend(touched.iter().cloned());
            s.sort();
            s.dedup();
            s
        }),
        _ => None,
    }
    .filter(|s| s.len() <= MAX_PATHSPECS);
    let full = specs.is_none();
    if state.worktree.as_ref().is_none_or(|w| !std::sync::Arc::ptr_eq(&w.index, &index)) {
        let ctx = WorktreeCtx::new(repo, &index)?;
        let (modified, conflicted) = match state.worktree.take() {
            Some(w) => (w.modified, w.conflicted),
            None => Default::default(),
        };
        state.worktree = Some(Worktree { index: index.clone(), ctx, modified, conflicted });
    }
    let wt = state.worktree.as_mut()?;
    if full {
        wt.modified.clear();
        wt.conflicted.clear();
    }
    let specs = specs.unwrap_or_default();
    state.stats.worktree_checked = (!full).then_some(specs.len());
    if full || !specs.is_empty() {
        for p in &specs {
            remove_under(&mut wt.modified, p.as_bstr());
            remove_under(&mut wt.conflicted, p.as_bstr());
        }
        let patterns: Vec<BString> = specs
            .iter()
            .map(|p| {
                let mut s = BString::from(":(literal)");
                s.extend_from_slice(p);
                s
            })
            .collect();
        wt.check(repo, &index, &patterns, full)?;
    }
    lap("index-worktree");

    let trust = match (&changes, &index_touched) {
        (Some(c), Some(t)) => Some(untracked::Trust::new(c, t)),
        _ => None,
    };
    let untracked = state.untracked.count(repo, &view, trust.as_ref())?;
    lap("untracked");

    let wt = state.worktree.as_ref()?;
    let staged = &state.staged.as_ref()?.1;
    let mut st = Status {
        staged: staged.iter().filter(|p| !wt.conflicted.contains(*p)).count() as u32,
        modified: wt.modified.len() as u32,
        untracked,
        conflicted: wt.conflicted.len() as u32,
        ..Default::default()
    };
    if let Some((a, b)) = ahead_behind {
        (st.ahead, st.behind) = (a, b);
    }
    lap("ahead-behind");
    Some(st)
}

/// What `index_as_worktree` needs besides the paths: building it per call
/// cost ~3.5ms in aports (mostly an icase table only the dirwalk uses).
struct WorktreeCtx {
    stack: gix::worktree::Stack,
    filter: gix::filter::plumbing::Pipeline,
    submodule: gix::status::index_worktree::BuiltinSubmoduleStatus,
    options: iaw::Options,
}

impl WorktreeCtx {
    fn new(repo: &gix::Repository, index: &gix::index::State) -> Option<Self> {
        let attrs = repo
            .attributes(
                index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
                None,
            )
            .ok()?;
        let cache = gix::diff::resource_cache(
            repo,
            gix::diff::blob::pipeline::Mode::ToGit,
            attrs.detach(),
            gix::diff::blob::pipeline::WorktreeRoots { old_root: None, new_root: repo.workdir().map(Into::into) },
        )
        .ok()?;
        let fscache = repo.config_snapshot().boolean("core.fscache").unwrap_or(false);
        Some(Self {
            stack: cache.attr_stack,
            filter: cache.filter.worktree_filter,
            submodule: gix::status::index_worktree::BuiltinSubmoduleStatus::new(
                repo.clone().into_sync(),
                Default::default(),
            )
            .ok()?,
            options: iaw::Options {
                fs: repo.filesystem_options().ok()?,
                thread_limit: None,
                stat: repo.stat_options().ok()?,
                fscache,
            },
        })
    }
}

impl Worktree {
    /// Re-checks index entries matching `patterns` (all if empty) against
    /// the worktree, adding hits to the sets.
    fn check(&mut self, repo: &gix::Repository, index: &gix::index::State, patterns: &[BString], full: bool) -> Option<()> {
        let pathspec = repo
            .pathspec(
                false,
                patterns,
                true,
                index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            )
            .ok()?
            .into_parts()
            .0;
        let mut collect = Collect { modified: &mut self.modified, conflicted: &mut self.conflicted };
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let mut options = self.ctx.options.clone();
        // A few paths: spawning a worker per core costs more than the check.
        options.thread_limit = (!full).then_some(1);
        gix::status::plumbing::index_as_worktree(
            index,
            repo.workdir()?,
            &mut collect,
            iaw::traits::FastEq,
            self.ctx.submodule.clone(),
            repo.objects.clone().into_arc().ok()?,
            &mut gix::progress::Discard,
            iaw::Context {
                pathspec,
                stack: self.ctx.stack.clone(),
                filter: self.ctx.filter.clone(),
                should_interrupt: &interrupt,
            },
            options,
        )
        .ok()?;
        Some(())
    }
}

/// Buckets entries like `git status`: conflicts, and any other change
/// (intent-to-add shows as `.A`) except stat-only refreshes as modified.
struct Collect<'a> {
    modified: &'a mut BTreeSet<BString>,
    conflicted: &'a mut BTreeSet<BString>,
}

impl<'index> iaw::VisitEntry<'index> for Collect<'_> {
    type ContentChange = ();
    type SubmoduleStatus = gix::submodule::Status;

    fn visit_entry(
        &mut self,
        _entries: &'index [gix::index::Entry],
        _entry: &'index gix::index::Entry,
        _entry_index: usize,
        rela_path: &'index BStr,
        status: EntryStatus<(), gix::submodule::Status>,
    ) {
        match status {
            EntryStatus::Conflict { .. } => {
                self.conflicted.insert(rela_path.to_owned());
            }
            EntryStatus::Change(_) | EntryStatus::IntentToAdd => {
                self.modified.insert(rela_path.to_owned());
            }
            EntryStatus::NeedsUpdate(_) => {}
        }
    }
}

/// Paths staged in `index` relative to `tree`, renames tracked like git.
fn staged(
    repo: &gix::Repository,
    tree: &gix::index::State,
    index: &gix::index::State,
    rewrites: Option<gix::diff::Rewrites>,
) -> Option<Vec<BString>> {
    let mut cache = match rewrites {
        Some(_) => Some(repo.diff_resource_cache_for_tree_diff().ok()?),
        None => None,
    };
    let mut search = repo
        .pathspec(
            true,
            None::<&str>,
            false,
            &gix::index::State::new(repo.object_hash()),
            gix::worktree::stack::state::attributes::Source::IdMapping,
        )
        .ok()?
        .into_parts()
        .0;
    let mut out = Vec::new();
    gix::diff::index(
        tree,
        index,
        |change| {
            out.push(change.location().to_owned());
            Ok(std::ops::ControlFlow::Continue(()))
        },
        rewrites.zip(cache.as_mut()).map(|(rewrites, resource_cache)| gix::diff::index::RewriteOptions {
            resource_cache,
            find: repo,
            rewrites,
        }),
        &mut search,
        &mut |_, _, _, _| false,
    )
    .ok()?;
    Some(out)
}

#[cfg(test)]
pub(crate) static CACHE_TREE_HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Rename tracking for staged changes, configured like `git status`.
/// Outer `None`: unreadable config.
fn rewrites(repo: &gix::Repository) -> Option<Option<gix::diff::Rewrites>> {
    let cfg = repo.config_snapshot();
    Some(match cfg.boolean("status.renames") {
        Some(false) => None,
        Some(true) => Some(Default::default()),
        None => match gix::diff::new_rewrites(cfg.plumbing(), true).ok()? {
            (r, true) => r,
            (_, false) => Some(Default::default()),
        },
    })
}

/// Staged paths using the index cache-tree like git: only subtrees git
/// marked as changed are compared with HEAD's trees (right after a commit
/// nothing is). `None` when rename tracking could pair an add with a delete,
/// or the index has entries this does not model (conflicts, intent-to-add,
/// sparse); the caller then does a full diff.
fn staged_via_cache_tree(
    repo: &gix::Repository,
    index: &gix::index::State,
    tree: gix::ObjectId,
    rewrites: Option<&gix::diff::Rewrites>,
) -> Option<Vec<BString>> {
    if index.is_sparse() {
        return None;
    }
    let root = index.tree()?;
    let mut w = CacheTreeWalk { repo, index, out: Vec::new(), adds: 0, dels: 0 };
    w.dir(b"".as_bstr(), Some(root), tree)?;
    let pairable = match rewrites {
        None => false,
        Some(r) if r.copies.is_some() => w.adds > 0,
        Some(_) => w.adds > 0 && w.dels > 0,
    };
    (!pairable).then_some(w.out)
}

struct CacheTreeWalk<'a> {
    repo: &'a gix::Repository,
    index: &'a gix::index::State,
    out: Vec<BString>,
    adds: u32,
    dels: u32,
}

impl CacheTreeWalk<'_> {
    fn dir(&mut self, path: &BStr, ct: Option<&gix::index::extension::Tree>, tree: gix::ObjectId) -> Option<()> {
        if ct.is_some_and(|c| c.num_entries.is_some() && c.id == tree) {
            return Some(());
        }
        use gix::object::tree::EntryKind;
        let head: Vec<(BString, EntryKind, gix::ObjectId)> = self
            .repo
            .find_tree(tree)
            .ok()?
            .decode()
            .ok()?
            .entries
            .iter()
            .map(|e| (e.filename.to_owned(), e.mode.kind(), e.oid.to_owned()))
            .collect();
        let join = |name: &BStr| -> BString {
            let mut p = path.to_owned();
            if !p.is_empty() {
                p.push(b'/');
            }
            p.extend_from_slice(name);
            p
        };
        let prefix = join(b"".as_bstr());
        let entries = self.index.entries();
        let range = if path.is_empty() {
            0..entries.len()
        } else {
            self.index.prefixed_entries_range(prefix.as_bstr()).unwrap_or(0..0)
        };
        let mut files = std::collections::BTreeMap::<&BStr, &gix::index::Entry>::new();
        let mut dirs = BTreeSet::<&BStr>::new();
        for e in &entries[range.clone()] {
            if e.stage_raw() != 0 || e.flags.contains(gix::index::entry::Flags::INTENT_TO_ADD) {
                return None;
            }
            let rest = e.path(self.index)[prefix.len()..].as_bstr();
            match rest.find_byte(b'/') {
                Some(i) => _ = dirs.insert(rest[..i].as_bstr()),
                None => _ = files.insert(rest, e),
            }
        }
        for (name, kind, oid) in &head {
            let name = name.as_bstr();
            if *kind == EntryKind::Tree {
                if files.contains_key(name) {
                    return None; // dir replaced by a file
                }
                if dirs.remove(name) {
                    let child = ct.and_then(|c| c.children.iter().find(|c| c.name.as_slice() == name.as_bytes()));
                    self.dir(join(name).as_bstr(), child, *oid)?;
                } else {
                    self.all_deleted(join(name), *oid)?;
                }
                continue;
            }
            match files.remove(name) {
                Some(e) => {
                    if e.id != *oid || index_kind(e.mode) != Some(*kind) {
                        self.out.push(join(name));
                    }
                }
                None if dirs.contains(name) => return None, // file replaced by a dir
                None => {
                    self.out.push(join(name));
                    self.dels += 1;
                }
            }
        }
        for name in files.keys() {
            self.out.push(join(name));
            self.adds += 1;
        }
        for name in dirs {
            let mut p = join(name);
            p.push(b'/');
            let r = self.index.prefixed_entries_range(p.as_bstr()).unwrap_or(0..0);
            for e in &entries[r] {
                self.out.push(e.path(self.index).to_owned());
                self.adds += 1;
            }
        }
        Some(())
    }

    /// Every blob below `tree`, which the index no longer has.
    fn all_deleted(&mut self, path: BString, tree: gix::ObjectId) -> Option<()> {
        let t = self.repo.find_tree(tree).ok()?;
        for e in t.decode().ok()?.entries {
            let mut p = path.clone();
            p.push(b'/');
            p.extend_from_slice(e.filename);
            if e.mode.is_tree() {
                self.all_deleted(p, e.oid.to_owned())?;
            } else {
                self.out.push(p);
                self.dels += 1;
            }
        }
        Some(())
    }
}

fn index_kind(mode: Mode) -> Option<gix::object::tree::EntryKind> {
    use gix::object::tree::EntryKind;
    Some(match mode {
        Mode::FILE => EntryKind::Blob,
        Mode::FILE_EXECUTABLE => EntryKind::BlobExecutable,
        Mode::SYMLINK => EntryKind::Link,
        Mode::COMMIT => EntryKind::Commit,
        _ => return None,
    })
}

/// Paths whose index entries differ between `old` and `new` (added, removed,
/// or changed id, mode, stage or stat). `None` past `limit`.
fn index_changes(old: &gix::index::State, new: &gix::index::State, limit: usize) -> Option<Vec<BString>> {
    let (a, b) = (old.entries(), new.entries());
    let key = |s: &gix::index::State, e: &gix::index::Entry| (e.path(s).to_owned(), e.stage_raw());
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        let ord = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) => (x.path(old), x.stage_raw()).cmp(&(y.path(new), y.stage_raw())),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        match ord {
            std::cmp::Ordering::Less => {
                out.push(key(old, &a[i]).0);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(key(new, &b[j]).0);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (x, y) = (&a[i], &b[j]);
                if x.id != y.id || x.mode != y.mode || x.flags != y.flags || x.stat != y.stat {
                    out.push(key(new, y).0);
                }
                i += 1;
                j += 1;
            }
        }
        if out.len() > limit {
            return None;
        }
    }
    out.dedup();
    Some(out)
}

/// Index paths (files, dirs with tracked files, submodules) touched by `changes`,
/// spelled as in the index. `None` if a change hits the git dir itself.
fn tracked_specs(index: &IndexView, changes: &HashSet<BString>) -> Option<Vec<BString>> {
    let mut out = BTreeSet::new();
    for p in changes {
        let p = p.as_bstr();
        if let Some(e) = index.entry(p) {
            out.insert(e.path(index.state).to_owned());
        } else if let Some(prefix) = index.dir_spelling(p) {
            out.insert(prefix);
        }
        // Changes inside a submodule affect its entry.
        let mut a = p;
        while let Some(i) = a.rfind_byte(b'/') {
            a = a[..i].as_bstr();
            if let Some(e) = index.entry(a).filter(|e| e.mode == Mode::COMMIT) {
                out.insert(e.path(index.state).to_owned());
            }
        }
    }
    Some(out.into_iter().collect())
}

/// Removes `p` and everything below it.
fn remove_under(set: &mut BTreeSet<BString>, p: &BStr) {
    set.remove(p);
    let mut prefix = p.to_owned();
    prefix.push(b'/');
    let doomed: Vec<BString> =
        set.range(prefix.clone()..).take_while(|k| k.starts_with(&prefix)).cloned().collect();
    for k in doomed {
        set.remove(&k);
    }
}

/// Values derived from refs, revalidated by `stat` of the files they were
/// read from: HEAD, the branch and its upstream (loose), packed-refs. Reading
/// them costs 0.4-5ms here (commit objects, rev walk).
struct Refs {
    head: FileKey,
    head_name: Option<gix::refs::FullName>,
    key: [FileKey; 3],
    tree: gix::ObjectId,
    ahead_behind: Option<(u32, u32)>,
}

fn refs(repo: &gix::Repository, cache: &mut Option<Refs>) -> Option<(gix::ObjectId, Option<(u32, u32)>)> {
    let head = file_key(&repo.git_dir().join("HEAD"));
    let head_name = match cache {
        Some(c) if c.head == head && head.is_some() => c.head_name.clone(),
        _ => repo.head_name().ok()?,
    };
    let upstream = head_name
        .as_ref()
        .and_then(|n| repo.branch_remote_tracking_ref_name(n.as_ref(), gix::remote::Direction::Fetch)?.ok());
    let cd = repo.common_dir();
    let loose = |n: Option<&gix::refs::FullName>| {
        n.and_then(|n| gix::path::from_bstr(n.as_bstr()).ok().map(|p| file_key(&cd.join(p)))).flatten()
    };
    let key = [file_key(&cd.join("packed-refs")), loose(head_name.as_ref()), loose(upstream.as_ref())];
    if let Some(c) = cache
        && c.head == head
        && head.is_some()
        && c.key == key
    {
        return Some((c.tree, c.ahead_behind));
    }
    let tree = repo.head_tree_id_or_empty().ok()?.detach();
    let ab = ahead_behind(repo);
    *cache = Some(Refs { head, head_name, key, tree, ahead_behind: ab });
    Some((tree, ab))
}

fn ahead_behind(repo: &gix::Repository) -> Option<(u32, u32)> {
    let mut head = repo.head_ref().ok()??;
    let upstream = head.remote_tracking_ref_name(gix::remote::Direction::Fetch)?.ok()?;
    let local = head.peel_to_id().ok()?.detach();
    let remote = repo.find_reference(upstream.as_ref()).ok()?.into_fully_peeled_id().ok()?.detach();
    if local == remote {
        return Some((0, 0));
    }
    let count = |from, hide| -> Option<u32> {
        Some(repo.rev_walk([from]).with_hidden([hide]).all().ok()?.filter_map(Result::ok).count() as u32)
    };
    Some((count(local, remote)?, count(remote, local)?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    pub(crate) struct TestRepo(pub PathBuf);

    impl TestRepo {
        pub(crate) fn sh(&self, script: &str) {
            let out = Command::new("sh")
                .args(["-ec", script])
                .current_dir(&self.0)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    /// Fresh `<tmp>/<name>/work` with a bare `origin` next to it.
    pub(crate) fn setup(name: &str) -> (PathBuf, TestRepo) {
        let root = std::env::temp_dir().join(format!("ashell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        TestRepo(root.clone())
            .sh("git init -q --bare -b main remote.git && git init -q -b main work && cd work && git remote add origin ../remote.git");
        (root.clone(), TestRepo(root.join("work")))
    }

    /// Worktree mutations covering every status bucket and cache edge case.
    pub(crate) const STEPS: &[(&str, &str)] = &[
        ("initial commit", "mkdir -p src/deep && echo a > a.txt && echo l > src/lib.rs && echo d > src/deep/d.rs && git add -A && git commit -qm init && git push -qu origin main"),
        ("untracked file", "touch new.txt"),
        ("untracked nested dir collapses", "mkdir -p newdir/sub && touch newdir/sub/x"),
        ("untracked file in tracked subdir", "touch src/deep/extra.rs"),
        ("gitignore created", "mkdir build && touch build/o && printf 'build/\\n' > .gitignore"),
        ("gitignore edited in place", "printf 'build/\\n*.txt\\n' > .gitignore"),
        ("dir with only ignored files", "mkdir logs && touch logs/a.txt"),
        ("empty dir", "mkdir emptydir"),
        ("nested repo", "git init -q nested"),
        ("nested gitignore", "printf '*.rs\\n' > src/.gitignore"),
        ("nested gitignore removed", "rm src/.gitignore"),
        ("modify tracked", "echo b >> src/lib.rs"),
        ("stage it", "git add src/lib.rs"),
        ("modify again", "echo c >> src/lib.rs"),
        ("revert worktree edit", "git checkout -q src/lib.rs"),
        ("edit then restore same content", "cp src/deep/d.rs /tmp/ashell-d.$$ && echo z >> src/deep/d.rs && cp /tmp/ashell-d.$$ src/deep/d.rs && rm /tmp/ashell-d.$$"),
        ("delete untracked", "rm -r newdir"),
        ("untracked deep under tracked", "mkdir -p src/deep/n1/n2 && touch src/deep/n1/n2/f"),
        ("move dir out, change it, move back", "mv src/deep/n1 ../n1 && touch ../n1/n2/g && mkdir ../n1/n3 && touch ../n1/n3/h && mv ../n1 src/deep/n1"),
        ("move tracked dir out", "mv src/deep ../deep"),
        ("move tracked dir back", "mv ../deep src/deep"),
        ("delete tracked", "rm src/deep/d.rs"),
        ("commit -> ahead", "git add -A src && git commit -qm two"),
        ("root gitignore drops a rule", "printf 'build/\\n' > .gitignore"),
        ("intent to add", "touch ita.md && git add -N ita.md"),
        ("case-only rename", "git mv a.txt A.txt"),
        ("untracked inside untracked-only dir", "mkdir -p only/a/b && touch only/a/b/f && git add -f only/a/b/f && git rm -q --cached only/a/b/f"),
        ("submodule added", "rm -rf nested && git add -A && git commit -qm wip && git -c protocol.file.allow=always submodule add -q ../remote.git sub && git commit -qm sub"),
        ("submodule untracked content", "touch sub/inner"),
        ("submodule staged content", "git -C sub add inner"),
        ("submodule new commit", "git -C sub commit -qm inner"),
        ("stage new file in new dir", "mkdir -p stg/a && echo n > stg/a/n.txt && git add stg"),
        ("commit it", "git commit -qm stg"),
        ("stage deletion", "git rm -q stg/a/n.txt"),
        ("stage rename", "git reset -q && git mv src/lib.rs src/lib2.rs"),
        ("stage exec bit", "chmod +x src/lib2.rs && git add src/lib2.rs"),
        ("dir replaced by file", "rm -rf src/deep && echo f > src/deep && git add -A src/deep"),
        ("commit staged", "git add -A && git commit -qm more"),
        ("stage edit deep", "mkdir -p stg/a && echo e > stg/a/n.txt && git add -A stg"),
        ("branch switch back and forth", "git add -A && git commit -qm sub && git checkout -qb side HEAD~2 2>/dev/null && touch side-only && git checkout -q main"),
        ("behind", "git reset -q && git stash -qu && echo base > src/lib2.rs && git commit -qam base && git push -q origin main && git reset -q --hard HEAD~1"),
        ("conflict", "echo x > src/lib2.rs && git commit -qam local && (git merge -q origin/main >/dev/null 2>&1 || true)"),
    ];

    /// Native (warm cache, mutated step by step) must equal `git status` after every step.
    #[test]
    fn native_matches_git_through_mutations() {
        let (root, r) = setup("gittest");
        let mut cache = State::default();
        let mut check = |step: &str| {
            let native = status(&r.0, &mut cache, Changes::Unknown);
            let cli = crate::git::cli_status(&r.0);
            assert_eq!(native, cli, "step: {step}");
        };
        check("fresh");
        let (mut behind, mut conflicted) = (false, false);
        for (name, script) in STEPS {
            r.sh(script);
            check(name);
            let cli = crate::git::cli_status(&r.0).unwrap();
            behind |= cli.behind > 0;
            conflicted |= cli.conflicted > 0;
        }
        assert!(behind && conflicted, "steps must cover behind and conflicted");
        let hits = CACHE_TREE_HITS.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("cache-tree staged diffs: {hits}");
        assert!(hits > 5, "cache-tree path barely exercised");
        let _ = std::fs::remove_dir_all(&root);
    }
}

