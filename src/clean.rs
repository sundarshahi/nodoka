//! Frees disk in idle git worktrees under `clean_dirs`: coding agents make a worktree per task,
//! install 1–2 GB of dependencies into it, and leave it behind. Runs on its own thread, hourly.
//!
//! Only folders git ignores and a tool rebuilds are deleted. A whole worktree goes only when
//! nothing in it is lost: no changes, its commit is on a remote, no other ignored files.

use crate::sys;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Rebuilt by an install or the next build.
const CACHES: &[&str] = &["node_modules", ".next", ".turbo", ".nuxt", ".svelte-kit", ".parcel-cache", ".vite"];

pub struct Rules {
    pub dirs: Vec<PathBuf>,
    /// Idle this long → delete its caches. 0: never.
    pub caches_after_s: u64,
    /// Idle this long → remove the worktree, if nothing would be lost. 0: never.
    pub worktrees_after_s: u64,
    pub dry_run: bool,
}

/// One pass. `in_use`: folders a running process works in or names on its command line.
/// Returns log lines.
pub fn pass(r: &Rules, in_use: &[String], now: u64) -> Vec<String> {
    let mut out = vec![];
    for dir in &r.dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for w in entries.flatten().map(|e| e.path()) {
            let Some(gitdir) = linked_gitdir(&w) else { continue };
            let idle = now.saturating_sub(last_git_activity(&gitdir));
            let wp = w.to_string_lossy();
            let busy = in_use.iter().any(|u| Path::new(u).starts_with(&w));
            if busy || r.caches_after_s == 0 || idle < r.caches_after_s {
                continue;
            }
            let (caches, others) = ignored(&w);
            for c in &caches {
                let p = w.join(c);
                let size = du(&p);
                if size == 0 {
                    continue;
                }
                if !r.dry_run && fs::remove_dir_all(&p).is_err() {
                    continue;
                }
                let verb = if r.dry_run { "would free" } else { "freed" };
                out.push(format!(
                    "clean           {verb} {} in {} — {c} (idle {}h)",
                    gb(size),
                    w.display(),
                    idle / 3600
                ));
            }
            if r.worktrees_after_s == 0 || idle < r.worktrees_after_s {
                continue;
            }
            if let Some(why) = keep_reason(&w, &others) {
                if r.dry_run {
                    out.push(format!("clean           keeps worktree {}: {why}", w.display()));
                }
                continue;
            }
            let size = du(&w);
            if r.dry_run {
                out.push(format!("clean           would remove worktree {} ({})", w.display(), gb(size)));
            } else if git(&w, &["worktree", "remove", &wp]).is_some() {
                out.push(format!(
                    "clean           removed worktree {} ({}, idle {} days; its commit is on a remote)",
                    w.display(),
                    gb(size),
                    idle / 86400
                ));
            }
        }
    }
    out
}

/// The gitdir of a linked worktree (a `.git` file, not a folder: never a main checkout).
fn linked_gitdir(w: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(w.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    gitdir.is_dir().then_some(gitdir)
}

/// Last checkout, commit, `git status` or add in this worktree (seconds since the epoch).
fn last_git_activity(gitdir: &Path) -> u64 {
    ["index", "HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|f| fs::metadata(gitdir.join(f)).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(u64::MAX) // unknown: never idle
}

/// Ignored paths, split into rebuildable caches and everything else (`.env`, build output…).
fn ignored(w: &Path) -> (Vec<String>, Vec<String>) {
    let out = git(w, &["status", "--porcelain", "--ignored=matching"]).unwrap_or_default();
    // `--untracked-files=no` would hide ignored files too: a worktree with a .env looked empty
    out.lines()
        .filter_map(|l| l.strip_prefix("!! "))
        .map(|p| p.trim_end_matches('/').to_string())
        .partition(|p| CACHES.contains(&p.rsplit('/').next().unwrap_or(p)))
}

fn keep_reason(w: &Path, others: &[String]) -> Option<String> {
    if let Some(f) = others.first() {
        return Some(format!("ignored files would be lost ({f})"));
    }
    match git(w, &["status", "--porcelain"]) {
        None => return Some("git status failed".into()),
        Some(s) if !s.trim().is_empty() => return Some("uncommitted changes".into()),
        _ => {}
    }
    let on_remote = git(w, &["for-each-ref", "--contains", "HEAD", "--count=1", "refs/remotes"]);
    if on_remote.is_none_or(|s| s.trim().is_empty()) {
        return Some("its commit isn't on a remote".into());
    }
    None
}

fn git(w: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(w).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Bytes on disk under `p`, symlinks not followed.
fn du(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else { return 0 };
    if !m.is_dir() {
        return m.len();
    }
    fs::read_dir(p).map_or(0, |es| es.flatten().map(|e| du(&e.path())).sum())
}

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
}

/// Working folders and command-line paths of this user's processes.
pub fn in_use() -> Vec<String> {
    let mut seen = HashSet::new();
    for p in sys::procs(sys::uid()) {
        if let Some(c) = sys::cwd(p.pid) {
            seen.insert(c.to_string_lossy().into_owned());
        }
        if let Some((argv, _)) = sys::args_env(p.pid) {
            seen.extend(argv.into_iter().filter(|a| Path::new(a).is_absolute()));
        }
    }
    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
        assert!(ok.status.success(), "{cmd}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    #[test]
    fn a_worktree_is_in_use_when_a_path_inside_it_is_whatever_the_separator() {
        let w = std::env::temp_dir().join("trees").join("app");
        let inside = w.join("src").to_string_lossy().replace('\\', "/");
        assert!(Path::new(&inside).starts_with(&w));
        assert!(
            !Path::new(&format!("{}-two/src", w.display())).starts_with(&w),
            "a sibling with a longer name isn't it"
        );
    }

    #[test]
    #[cfg(unix)] // cleaning is off on Windows until it can see working folders
    fn idle_worktrees_lose_their_caches_and_go_only_when_nothing_is_lost() {
        let base = std::env::temp_dir().join(format!("nodoka-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (repo, trees) = (base.join("repo"), base.join("trees"));
        fs::create_dir_all(&trees).unwrap();
        sh(&base, "git init -q --bare remote.git && git init -q -b main repo");
        sh(
            &repo,
            "git config user.email t@t && git config user.name t && git config commit.gpgsign false \
             && printf 'node_modules/\\n.env\\n' > .gitignore && git add . && git commit -qm one \
             && git remote add origin ../remote.git && git push -q origin main",
        );
        // pushed: goes; unpushed commit: stays; has .env: stays; in use: untouched
        for (name, extra) in [
            ("pushed", ""),
            ("unpushed", "&& echo x > a && git add a && git -c user.email=t@t -c user.name=t commit -qm two"),
            ("has-env", "&& echo SECRET=1 > .env"),
            ("in-use", ""),
        ] {
            sh(&repo, &format!("git worktree add -q ../trees/{name} -b {name} main"));
            sh(&trees.join(name), &format!("mkdir -p node_modules/pkg && echo code > node_modules/pkg/i.js {extra}"));
        }
        let in_use = vec![trees.join("in-use").join("src").to_string_lossy().into_owned()];
        let rules =
            |dry_run| Rules { dirs: vec![trees.clone()], caches_after_s: 3600, worktrees_after_s: 86400, dry_run };
        let now = sys::now();

        assert!(pass(&rules(false), &in_use, now).is_empty(), "nothing is idle yet");
        let log = pass(&rules(true), &in_use, now + 2 * 86400);
        assert!(trees.join("pushed/node_modules").exists(), "a dry run deletes nothing: {log:?}");
        assert!(log.iter().any(|l| l.contains("would remove worktree") && l.contains("pushed")), "{log:?}");

        let log = pass(&rules(false), &in_use, now + 2 * 86400).join("\n");
        assert!(!trees.join("pushed").exists(), "clean, on the remote: removed\n{log}");
        for kept in ["unpushed", "has-env"] {
            assert!(trees.join(kept).join("a").exists() || trees.join(kept).exists(), "{kept} kept\n{log}");
            assert!(!trees.join(kept).join("node_modules").exists(), "{kept}: caches freed\n{log}");
        }
        assert!(trees.join("has-env/.env").exists(), "ignored files are never deleted");
        assert!(trees.join("in-use/node_modules/pkg/i.js").exists(), "in use: untouched");
        assert!(repo.join(".gitignore").exists(), "the main checkout is never touched");
        let _ = fs::remove_dir_all(&base);
    }
}
