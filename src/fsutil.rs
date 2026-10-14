//! Filesystem helpers: APFS clonefile, free space, symlink repair.

use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const CLONE_NOFOLLOW: u32 = 0x0001;

unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains NUL")
}

/// Clone a file, symlink or whole directory in one syscall. Data blocks are
/// shared copy-on-write, so this costs metadata only.
pub fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        let err = std::io::Error::last_os_error();
        bail!("clonefile {} -> {}: {err}", src.display(), dst.display());
    }
    Ok(())
}

/// Clone the children of `src` into the existing directory `dst`, skipping
/// `excludes` (paths relative to the root). A directory that only contains an
/// excluded path is recreated and its other children cloned one by one.
pub fn clone_tree(
    src_root: &Path,
    dst_root: &Path,
    rel: &Path,
    excludes: &[PathBuf],
) -> Result<usize> {
    let mut calls = 0;
    let dir = src_root.join(rel);
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let r = rel.join(entry.file_name());
        if excludes.iter().any(|e| e == &r) {
            continue;
        }
        let ft = entry.file_type()?;
        let has_nested_exclude = excludes.iter().any(|e| e.starts_with(&r) && e != &r);
        if ft.is_dir() && has_nested_exclude {
            let meta = entry.metadata()?;
            std::fs::create_dir(dst_root.join(&r))?;
            std::fs::set_permissions(dst_root.join(&r), meta.permissions())?;
            calls += clone_tree(src_root, dst_root, &r, excludes)?;
        } else {
            clone_path(&entry.path(), &dst_root.join(&r))?;
            calls += 1;
        }
    }
    Ok(calls)
}

/// Bytes available to this user on the volume holding `path`.
pub fn free_bytes(path: &Path) -> Result<u64> {
    let c = cstr(path)?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        bail!(
            "statvfs {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// Device id, to make sure source and workspaces share one APFS volume.
pub fn device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.dev())
}

/// What one pass over a fresh workspace found and fixed.
#[derive(Default)]
pub struct Scan {
    /// Absolute links into the source, now pointing into the workspace.
    pub retargeted: Vec<PathBuf>,
    /// Links that resolve outside the workspace (absolute, or relative with
    /// enough `..` to leave the root); writes through them escape it.
    pub external: Vec<(PathBuf, PathBuf)>,
    /// Untracked files matching the secret patterns, deleted.
    pub secrets: Vec<String>,
}

/// Remove `.`/`..` lexically. `None` when `..` climbs above the start.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// Walk the workspace once (skipping every `.git` file or folder):
/// - absolute symlinks into `src_root` are retargeted into `dst_root`;
/// - relative symlinks that climb out of the root are rewritten to the
///   absolute path they meant in the source, and reported as external;
/// - other absolute symlinks are reported as external;
/// - untracked files whose name matches `is_secret` are deleted.
pub fn scan(
    src_root: &Path,
    dst_root: &Path,
    tracked: &std::collections::HashSet<String>,
    is_secret: &dyn Fn(&str) -> bool,
) -> Result<Scan> {
    let mut found = Scan::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        for entry in std::fs::read_dir(dst_root.join(&rel_dir))? {
            let entry = entry?;
            let name = entry.file_name();
            let rel = rel_dir.join(&name);
            // The worktree's `.git` link and submodule git dirs are Git's own
            // data, never workspace content.
            if name == ".git" {
                continue;
            }
            let ft = entry.file_type()?;
            let p = entry.path();
            if ft.is_dir() {
                stack.push(rel);
            } else if ft.is_symlink() {
                let target = std::fs::read_link(&p)?;
                if let Ok(rest) = target.strip_prefix(src_root) {
                    relink(&p, &dst_root.join(rest))?;
                    found.retargeted.push(rel);
                } else if target.is_absolute() {
                    found.external.push((rel, target));
                } else if normalize(&rel_dir.join(&target)).is_none() {
                    let meant = normalize(&src_root.join(&rel_dir).join(&target)).unwrap_or(target);
                    relink(&p, &meant)?;
                    found.external.push((rel, meant));
                }
            } else if is_secret(&name.to_string_lossy())
                && !tracked.contains(&*rel.to_string_lossy())
            {
                std::fs::remove_file(&p)?;
                found.secrets.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    found.retargeted.sort();
    found.external.sort();
    found.secrets.sort();
    Ok(found)
}

fn relink(link: &Path, target: &Path) -> Result<()> {
    std::fs::remove_file(link)?;
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

/// Shell-style match of a whole name: `*` is any run, `?` one character.
pub fn glob(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_whole_names() {
        assert!(glob(".env", ".env"));
        assert!(glob(".env.*", ".env.local"));
        assert!(!glob(".env.*", ".env"));
        assert!(glob("*.pem", "server.pem"));
        assert!(!glob("*.pem", "server.pem.bak"));
        assert!(glob("id_?sa", "id_rsa"));
        assert!(glob("*a*b*", "xaybz"));
        assert!(!glob("a", "ab"));
    }

    #[test]
    fn normalize_detects_escapes() {
        assert_eq!(normalize(Path::new("a/../b")), Some(PathBuf::from("b")));
        assert_eq!(normalize(Path::new("a/./b/..")), Some(PathBuf::from("a")));
        assert_eq!(normalize(Path::new("../x")), None);
        assert_eq!(normalize(Path::new("a/../../x")), None);
    }
}
