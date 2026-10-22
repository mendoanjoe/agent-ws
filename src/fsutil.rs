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

/// `ATTR_CMNEXT_PRIVATESIZE` from `<sys/attr.h>` (not in the libc crate).
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;

/// Bytes of a file's data that no other file shares, as APFS counts them.
/// A fresh clone has 0; a file written after cloning, or whose clone
/// siblings were all deleted, owns its blocks. `None` if APFS cannot say.
pub fn private_size(path: &Path) -> Option<u64> {
    let c = cstr(path).ok()?;
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.forkattr = ATTR_CMNEXT_PRIVATESIZE;
    // u32 length, then the off_t.
    let mut buf = [0u8; 16];
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if rc != 0 || u32::from_ne_bytes(buf[..4].try_into().ok()?) < 12 {
        return None;
    }
    Some(i64::from_ne_bytes(buf[4..12].try_into().ok()?).max(0) as u64)
}

/// Disk use of a tree, counting regular files only.
#[derive(Default, Clone, Copy, serde::Serialize)]
pub struct Usage {
    pub files: u64,
    /// Sum of file sizes (what `du -A` shows).
    pub logical: u64,
    /// Allocated blocks, counting shared blocks in full (what `du` shows).
    pub allocated: u64,
    /// Blocks no other file shares: what deleting the tree frees for sure.
    pub private: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.files += o.files;
        self.logical += o.logical;
        self.allocated += o.allocated;
        self.private += o.private;
    }
}

/// Walk `root` without following symlinks and add up its files.
pub fn usage(root: &Path) -> Usage {
    usage_by(root, 0).0
}

/// Like `usage`, plus the usage per path `depth` levels below `root`
/// (`depth` 1 = top-level entries), for finding what grows.
pub fn usage_by(root: &Path, depth: usize) -> (Usage, Vec<(PathBuf, Usage)>) {
    use std::os::unix::fs::MetadataExt;
    let mut total = Usage::default();
    let mut parts: std::collections::BTreeMap<PathBuf, Usage> = Default::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        // Folders can vanish mid-walk (a build or a delete running).
        let Ok(rd) = std::fs::read_dir(root.join(&rel_dir)) else {
            continue;
        };
        for entry in rd.flatten() {
            let rel = rel_dir.join(entry.file_name());
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(rel);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            let p = entry.path();
            let u = Usage {
                files: 1,
                logical: md.len(),
                allocated: md.blocks() * 512,
                private: private_size(&p).unwrap_or(md.blocks() * 512),
            };
            total += u;
            if depth > 0 {
                let key: PathBuf = rel.components().take(depth).collect();
                *parts.entry(key).or_default() += u;
            }
        }
    }
    (total, parts.into_iter().collect())
}

/// Processes of this user whose current directory is inside `root`
/// (canonical path), as (pid, cwd). Agents and shells sit in their workspace.
pub fn cwd_users(root: &Path) -> Vec<(u32, PathBuf)> {
    let mut pids = vec![0 as libc::c_int; 8192];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    let mut out = Vec::new();
    for &pid in pids.iter().take(n.max(0) as usize) {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if got != size {
            continue;
        }
        let raw = info.pvi_cdir.vip_path.as_flattened();
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        let bytes: Vec<u8> = raw[..len].iter().map(|&c| c as u8).collect();
        let cwd = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
        if cwd.starts_with(root) {
            out.push((pid as u32, cwd));
        }
    }
    out
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

    /// Write and flush: APFS allocates blocks lazily, and unallocated data
    /// has no private size yet.
    fn write_synced(p: &Path, data: &[u8]) {
        use std::io::Write;
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(data).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn private_size_counts_only_unshared_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write_synced(&src.join("big"), &vec![7u8; 1 << 20]);
        let alone = private_size(&src.join("big")).unwrap();
        assert!(alone >= 1 << 20, "a lone file owns its blocks: {alone}");

        clone_path(&src, &tmp.path().join("copy")).unwrap();
        let copy = tmp.path().join("copy/big");
        assert_eq!(private_size(&copy), Some(0), "a fresh clone shares all");
        assert_eq!(usage(&tmp.path().join("copy")).private, 0);

        // Rewriting the clone gives it its own blocks; the original is
        // then alone again.
        write_synced(&copy, &vec![9u8; 1 << 20]);
        let u = usage(&tmp.path().join("copy"));
        assert!(u.private >= 1 << 20 && u.allocated >= u.private);
        assert_eq!((u.files, u.logical), (1, 1 << 20));
        assert!(private_size(&src.join("big")).unwrap() >= 1 << 20);
    }

    #[test]
    fn cwd_users_finds_processes_inside_a_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        // proc_pidinfo sees the cwd as soon as the process exists.
        let found = cwd_users(&dir).iter().any(|(p, _)| *p == child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(found);
        assert!(cwd_users(&dir.join("nothing-here")).is_empty());
    }

    #[test]
    fn normalize_detects_escapes() {
        assert_eq!(normalize(Path::new("a/../b")), Some(PathBuf::from("b")));
        assert_eq!(normalize(Path::new("a/./b/..")), Some(PathBuf::from("a")));
        assert_eq!(normalize(Path::new("../x")), None);
        assert_eq!(normalize(Path::new("a/../../x")), None);
    }
}
