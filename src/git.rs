//! Git branch detection without spawning `git`: reads `.git/HEAD`, walking up parents and
//! following worktree-style `.git` files.

use std::path::{Path, PathBuf};

const MAX_PARENT_WALK: usize = 6;

/// Returns the current branch (or short detached SHA) for `cwd`, if inside a repo.
pub fn branch_for(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    for _ in 0..=MAX_PARENT_WALK {
        let d = dir?;
        if let Some(git_dir) = git_dir_at(d) {
            return read_head(&git_dir);
        }
        dir = d.parent();
    }
    None
}

/// Basename of the repository checkout containing `cwd` (the directory holding `.git`).
pub fn repo_basename(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    for _ in 0..=MAX_PARENT_WALK {
        let d = dir?;
        if git_dir_at(d).is_some() {
            return d.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        dir = d.parent();
    }
    None
}

/// Resolves `<dir>/.git` to the directory holding `HEAD` (handles worktree `.git` files).
fn git_dir_at(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(target);
    Some(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

fn read_head(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!name.is_empty()).then(|| name.to_string());
    }
    if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(head[..7].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "herdr-autolabel-git-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn ref_head_yields_branch_and_walks_up() {
        let root = tmp("ref");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/HEAD"),
            "ref: refs/heads/feat/moving-button\n",
        )
        .unwrap();
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(branch_for(&root).as_deref(), Some("feat/moving-button"));
        assert_eq!(branch_for(&deep).as_deref(), Some("feat/moving-button"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detached_head_yields_short_sha() {
        let root = tmp("sha");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(branch_for(&root).as_deref(), Some("0123456"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worktree_git_file_is_followed() {
        let root = tmp("wt");
        let common = root.join("main/.git/worktrees/wt1");
        std::fs::create_dir_all(&common).unwrap();
        std::fs::write(common.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();
        let wt = root.join("wt1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", common.display())).unwrap();
        assert_eq!(branch_for(&wt).as_deref(), Some("wt-branch"));
        // Relative gitdir too.
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt1\n").unwrap();
        assert_eq!(branch_for(&wt).as_deref(), Some("wt-branch"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_repo_is_none_and_walk_is_bounded() {
        let root = tmp("none");
        let deep = root.join("1/2/3/4/5/6/7/8");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(branch_for(&root), None);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        // 8 levels down is beyond the 6-parent walk.
        assert_eq!(branch_for(&deep), None);
        assert_eq!(
            branch_for(&root.join("1/2/3/4/5/6")).as_deref(),
            Some("main")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
