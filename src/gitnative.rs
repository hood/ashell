//! In-process git status via gitoxide: parallel index/worktree stat walk,
//! HEAD-tree vs index diff and upstream ahead/behind, no subprocess.
//! Counts mirror `git status --porcelain=v2`.

use std::collections::HashSet;
use std::path::Path;

use gix::bstr::BString;
use gix::status::index_worktree::iter::Summary;
use gix::status::{Item, UntrackedFiles};

use crate::git::Status;
use crate::untracked;

/// `cache` persists untracked-scan state across calls (see `untracked`).
pub fn status(workdir: &Path, cache: &mut untracked::Cache) -> Option<Status> {
    let repo = gix::open(workdir).ok()?;
    let index = repo.index_or_empty().ok()?;
    let mut st = Status::default();
    let mut conflicted: HashSet<BString> = HashSet::new();
    let mut staged: Vec<BString> = Vec::new();

    let iter = repo
        .status(gix::progress::Discard)
        .ok()?
        .untracked_files(UntrackedFiles::None)
        .index_worktree_rewrites(None)
        .index(index.clone().into())
        .into_iter(Vec::<BString>::new())
        .ok()?;
    for item in iter {
        match item.ok()? {
            Item::IndexWorktree(iw) => match iw.summary() {
                Some(Summary::Conflict) => {
                    conflicted.insert(iw.rela_path().to_owned());
                }
                Some(Summary::Added) => {}
                Some(_) => st.modified += 1,
                None => {}
            },
            Item::TreeIndex(change) => staged.push(change.location().to_owned()),
        }
    }
    st.untracked = cache.count(&repo, &index)?;
    st.conflicted = conflicted.len() as u32;
    st.staged = staged.iter().filter(|p| !conflicted.contains(*p)).count() as u32;
    if let Some((a, b)) = ahead_behind(&repo) {
        (st.ahead, st.behind) = (a, b);
    }
    Some(st)
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
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    struct Repo(PathBuf);

    impl Repo {
        fn sh(&self, script: &str) {
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

    /// Native (warm cache, mutated step by step) must equal `git status` after every step.
    #[test]
    fn native_matches_git_through_mutations() {
        let root = std::env::temp_dir().join(format!("ashell-gittest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let remote = Repo(root.clone());
        remote.sh("git init -q --bare remote.git && git init -q -b main work && cd work && git remote add origin ../remote.git");
        let r = Repo(root.join("work"));
        let mut cache = untracked::Cache::default();
        let mut check = |step: &str| {
            let native = status(&r.0, &mut cache);
            let cli = crate::git::cli_status(&r.0);
            assert_eq!(native, cli, "step: {step}");
        };

        let steps: &[(&str, &str)] = &[
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
            ("delete untracked", "rm -r newdir"),
            ("delete tracked", "rm src/deep/d.rs"),
            ("commit -> ahead", "git add -A src && git commit -qm two"),
            ("intent to add", "touch ita.md && git add -N ita.md"),
            ("case-only rename", "git mv a.txt A.txt"),
            ("untracked inside untracked-only dir", "mkdir -p only/a/b && touch only/a/b/f && git add -f only/a/b/f && git rm -q --cached only/a/b/f"),
        ];
        check("fresh");
        for (name, script) in steps {
            r.sh(script);
            check(name);
        }

        // behind + conflict
        r.sh("git reset -q && git stash -qu && git push -q origin main && git reset -q --hard HEAD~1");
        check("behind");
        assert!(crate::git::cli_status(&r.0).unwrap().behind > 0, "behind step must be behind");
        r.sh("echo x > src/lib.rs && git commit -qam local && (git merge -q origin/main >/dev/null 2>&1 || true)");
        check("conflict");
        assert!(crate::git::cli_status(&r.0).unwrap().conflicted > 0, "conflict step must conflict");
        let _ = std::fs::remove_dir_all(&root);
    }
}
