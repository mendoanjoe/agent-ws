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
pub fn clone_tree(src_root: &Path, dst_root: &Path, rel: &Path, excludes: &[PathBuf]) -> Result<usize> {
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
        bail!("statvfs {}: {}", path.display(), std::io::Error::last_os_error());
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// Device id, to make sure source and workspaces share one APFS volume.
pub fn device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.dev())
}

/// Symlinks found while scanning a workspace.
#[derive(Default)]
pub struct Symlinks {
    /// Absolute links into the source, now pointing into the workspace.
    pub retargeted: Vec<PathBuf>,
    /// Absolute links outside the source; writes through them escape the
    /// workspace, so callers should warn.
    pub external: Vec<(PathBuf, PathBuf)>,
}

/// Rewrite absolute symlinks that point into `src_root` so they point into
/// `dst_root` instead. Without this, writing through such a link from a
/// workspace would change the source repository.
pub fn retarget_symlinks(src_root: &Path, dst_root: &Path) -> Result<Symlinks> {
    let mut found = Symlinks::default();
    let mut stack = vec![dst_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_symlink() {
                let target = std::fs::read_link(&p)?;
                let rel = p.strip_prefix(dst_root).unwrap_or(&p).to_path_buf();
                if let Ok(rest) = target.strip_prefix(src_root) {
                    std::fs::remove_file(&p)?;
                    std::os::unix::fs::symlink(dst_root.join(rest), &p)?;
                    found.retargeted.push(rel);
                } else if target.is_absolute() {
                    found.external.push((rel, target));
                }
            }
        }
    }
    Ok(found)
}

/// Basename match for secret files: `.env`, `.env.*` (not `.env.example`), `.dev.vars`.
pub fn is_secret(name: &str) -> bool {
    (name == ".env" || name.starts_with(".env.") || name == ".dev.vars") && name != ".env.example"
}
