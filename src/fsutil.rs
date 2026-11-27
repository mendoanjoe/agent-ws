//! Filesystem helpers: copy-on-write clones (APFS clonefile on macOS,
//! reflinks on Linux Btrfs/XFS), disk use, free space, symlink repair.

use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x0001;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains NUL")
}

/// Clone a file, symlink or whole directory in one syscall. Data blocks are
/// shared copy-on-write, so this costs metadata only.
#[cfg(target_os = "macos")]
pub fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        let err = std::io::Error::last_os_error();
        bail!("clonefile {} -> {}: {err}", src.display(), dst.display());
    }
    Ok(())
}

/// Clone a file, symlink or whole directory. Linux has no directory clone,
/// so directories are recreated and every file is reflinked (`FICLONE`):
/// data blocks are shared copy-on-write, as with APFS clones. Modes and
/// timestamps are kept, so build tools see the files as unchanged. Sockets,
/// pipes and devices are skipped. Fails on a filesystem without reflinks
/// (ext4): only Btrfs and XFS (with reflink=1, the default) have them.
#[cfg(target_os = "linux")]
pub fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    let mut dirs: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    while let Some((s, d)) = stack.pop() {
        let md = std::fs::symlink_metadata(&s).with_context(|| format!("stat {}", s.display()))?;
        let ft = md.file_type();
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&s)?, &d)
                .with_context(|| format!("symlink {}", d.display()))?;
        } else if ft.is_dir() {
            std::fs::create_dir(&d).with_context(|| format!("mkdir {}", d.display()))?;
            for entry in std::fs::read_dir(&s)? {
                let entry = entry?;
                stack.push((entry.path(), d.join(entry.file_name())));
            }
            dirs.push((d, md));
        } else if ft.is_file() {
            reflink(&s, &d, &md)?;
        }
    }
    // After the children: a read-only folder could not take them, and
    // creating them changed its mtime. Apply deepest-first.
    for (d, md) in dirs.into_iter().rev() {
        let _ = keep_times(&d, &md);
        std::fs::set_permissions(&d, md.permissions())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path, md: &std::fs::Metadata) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let from = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(md.permissions().mode())
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE as _, from.as_raw_fd()) } != 0 {
        let err = std::io::Error::last_os_error();
        drop(to);
        let _ = std::fs::remove_file(dst);
        bail!(
            "reflink {} -> {}: {err}; agent-ws needs a filesystem with reflinks (Btrfs, or XFS with reflink=1)",
            src.display(),
            dst.display()
        );
    }
    to.set_permissions(md.permissions())?;
    to.set_times(times(md)?)?;
    Ok(())
}

fn times(md: &std::fs::Metadata) -> Result<std::fs::FileTimes> {
    Ok(std::fs::FileTimes::new()
        .set_accessed(md.accessed()?)
        .set_modified(md.modified()?))
}

fn keep_times(dir: &Path, md: &std::fs::Metadata) -> Result<()> {
    std::fs::File::open(dir)?.set_times(times(md)?)?;
    Ok(())
}

fn normalize_excludes(excludes: &[PathBuf]) -> Vec<PathBuf> {
    excludes
        .iter()
        .filter_map(|e| normalize(e))
        .filter(|e| !e.as_os_str().is_empty())
        .collect()
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
    if rel.as_os_str().is_empty() {
        let norm = normalize_excludes(excludes);
        clone_tree_norm(src_root, dst_root, rel, &norm)
    } else {
        clone_tree_norm(src_root, dst_root, rel, excludes)
    }
}

fn clone_tree_norm(
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
            let entry_path = entry.path();
            let meta = std::fs::symlink_metadata(&entry_path)?;
            let dst_dir = dst_root.join(&r);
            std::fs::create_dir(&dst_dir)?;
            calls += clone_tree_norm(src_root, dst_root, &r, excludes)?;
            let _ = keep_times(&dst_dir, &meta);
            std::fs::set_permissions(&dst_dir, meta.permissions())?;
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
    Ok((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
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

/// Remove `.`/`..` lexically for relative paths. `None` when `..` climbs above the start.
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

/// Remove `.`/`..` lexically for absolute paths without popping `/`.
/// `None` if `..` climbs above `/` or the path is not absolute.
fn normalize_abs(p: &Path) -> Option<PathBuf> {
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::RootDir => out.push(c),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out.as_os_str() == "/" || !out.pop() {
                    return None;
                }
            }
            c => out.push(c),
        }
    }
    Some(out)
}

fn is_base64_pem_line(line: &[u8]) -> bool {
    if line.is_empty() {
        return false;
    }
    let mut pad = 0u8;
    for &b in line {
        if b == b'=' {
            pad += 1;
            if pad > 2 {
                return false;
            }
        } else if pad > 0 || !(b.is_ascii_alphanumeric() || b == b'+' || b == b'/') {
            return false;
        }
    }
    true
}

fn is_cert_metadata_line(raw_line: &[u8], trimmed: &[u8]) -> bool {
    if trimmed == b"Bag Attributes"
        || trimmed.starts_with(b"subject=")
        || trimmed.starts_with(b"issuer=")
    {
        return true;
    }
    // Indented OpenSSL bag attribute lines such as `    friendlyName: ...`.
    raw_line.starts_with(b" ") && trimmed.contains(&b':') && !trimmed.contains(&b'=')
}

/// Whether `path` is a regular file holding only PEM certificates (or
/// CRLs), comments allowed. Certificates are public: CA bundles such as
/// certifi's `cacert.pem` match `*.pem` by name, yet pip and requests fail
/// every HTTPS call without them. Keys, binary or unreadable files, and
/// anything mentioning a private key are not.
pub fn only_certificates(path: &Path) -> bool {
    use std::io::{BufRead, BufReader, Read};
    const LIMIT: u64 = 16 << 20;
    const MAX_LINE: usize = 64 * 1024;
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !md.is_file() || md.len() > LIMIT {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::with_capacity(8192, file.take(LIMIT));
    let mut raw_line = Vec::with_capacity(128);
    let mut in_block: Option<&'static [u8]> = None;
    let mut saw_b64 = false;
    let mut blocks = 0usize;

    loop {
        raw_line.clear();
        loop {
            let avail = match reader.fill_buf() {
                Ok(b) => b,
                Err(_) => return false,
            };
            if avail.is_empty() {
                break;
            }
            if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                if raw_line.len() + pos > MAX_LINE {
                    return false;
                }
                raw_line.extend_from_slice(&avail[..pos]);
                reader.consume(pos + 1);
                break;
            }
            if raw_line.len() + avail.len() > MAX_LINE {
                return false;
            }
            raw_line.extend_from_slice(avail);
            let n = avail.len();
            reader.consume(n);
            if reader.fill_buf().map(|b| b.is_empty()).unwrap_or(true) {
                break;
            }
        }
        if raw_line.is_empty() && reader.fill_buf().map(|b| b.is_empty()).unwrap_or(true) {
            break;
        }
        if raw_line
            .iter()
            .any(|&b| b == 0 || (b < 0x20 && b != b'\t' && b != b'\r'))
        {
            return false;
        }
        if raw_line.windows(11).any(|w| w == b"PRIVATE KEY") {
            return false;
        }
        let trimmed = raw_line.trim_ascii();
        if let Some(expected) = in_block {
            if let Some(label) = trimmed
                .strip_prefix(b"-----END ")
                .and_then(|l| l.strip_suffix(b"-----"))
            {
                if label != expected || !saw_b64 {
                    return false;
                }
                in_block = None;
                blocks += 1;
            } else if is_base64_pem_line(trimmed) {
                saw_b64 = true;
            } else {
                return false;
            }
        } else if trimmed.is_empty() || trimmed.starts_with(b"#") {
            continue;
        } else if let Some(label) = trimmed
            .strip_prefix(b"-----BEGIN ")
            .and_then(|l| l.strip_suffix(b"-----"))
        {
            in_block = match label {
                b"CERTIFICATE" => Some(b"CERTIFICATE"),
                b"TRUSTED CERTIFICATE" => Some(b"TRUSTED CERTIFICATE"),
                b"X509 CRL" => Some(b"X509 CRL"),
                _ => return false,
            };
            saw_b64 = false;
        } else if !is_cert_metadata_line(&raw_line, trimmed) {
            return false;
        }
    }
    in_block.is_none() && blocks > 0
}

/// Walk the workspace once (skipping every `.git` file or folder):
/// - untracked files and symlinks whose name matches `is_secret` are deleted
///   (regular files holding only certificates are spared);
/// - absolute symlinks into `src_root` are retargeted into `dst_root`;
/// - relative symlinks that climb out of the root are retargeted into
///   `dst_root` if they resolve back into `src_root`, or rewritten to the
///   absolute path they meant in the source and reported as external;
/// - other absolute symlinks are reported as external.
pub fn scan(
    src_root: &Path,
    dst_root: &Path,
    tracked: &std::collections::HashSet<String>,
    is_secret: &dyn Fn(&str) -> bool,
) -> Result<Scan> {
    let norm_src = normalize_abs(src_root);
    let src_cmp = norm_src.as_deref().unwrap_or(src_root);
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
                if is_secret(&name.to_string_lossy()) && !tracked.contains(&*rel.to_string_lossy())
                {
                    std::fs::remove_file(&p)?;
                    found.secrets.push(rel.to_string_lossy().into_owned());
                    continue;
                }
                let target = std::fs::read_link(&p)?;
                if target.is_absolute() {
                    if let Some(norm_t) = normalize_abs(&target)
                        && let Ok(rest) = norm_t.strip_prefix(src_cmp)
                        && let Some(clean_rest) = normalize(rest)
                    {
                        relink(&p, &dst_root.join(&clean_rest))?;
                        found.retargeted.push(rel);
                    } else {
                        found.external.push((rel, target));
                    }
                } else if normalize(&rel_dir.join(&target)).is_none() {
                    if let Some(meant) = normalize_abs(&src_cmp.join(&rel_dir).join(&target)) {
                        if let Ok(rest) = meant.strip_prefix(src_cmp)
                            && let Some(clean_rest) = normalize(rest)
                        {
                            relink(&p, &dst_root.join(&clean_rest))?;
                            found.retargeted.push(rel);
                        } else {
                            relink(&p, &meant)?;
                            found.external.push((rel, meant));
                        }
                    } else {
                        found.external.push((rel, target));
                    }
                }
            } else if is_secret(&name.to_string_lossy())
                && !tracked.contains(&*rel.to_string_lossy())
                && !only_certificates(&p)
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
#[cfg(target_os = "macos")]
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;

/// Bytes of a file's data that no other file shares, as APFS counts them.
/// A fresh clone has 0; a file written after cloning, or whose clone
/// siblings were all deleted, owns its blocks. `None` if APFS cannot say.
#[cfg(target_os = "macos")]
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

/// Bytes of a file's data that no other file shares: the extents `FIEMAP`
/// does not flag as shared (Btrfs and XFS flag reflinked ones). `None` if
/// the filesystem cannot say.
#[cfg(target_os = "linux")]
pub fn private_size(path: &Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    /// `struct fiemap_extent` from `<linux/fiemap.h>`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    /// `struct fiemap` with room for a batch of extents.
    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Extent; 64],
    }
    // _IOWR('f', 11, struct fiemap), the header being 32 bytes.
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

    let file = std::fs::File::open(path).ok()?;
    let mut private = 0;
    let mut start = 0;
    loop {
        let mut map: Fiemap = unsafe { std::mem::zeroed() };
        map.start = start;
        map.length = u64::MAX - start;
        map.flags = 0;
        map.count = 64;
        if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &raw mut map) } != 0 {
            return None;
        }
        if map.mapped == 0 {
            return Some(private);
        }
        for e in &map.extents[..(map.mapped as usize).min(64)] {
            if e.flags & FIEMAP_EXTENT_SHARED == 0 {
                private += e.length;
            }
            if e.flags & FIEMAP_EXTENT_LAST != 0 {
                return Some(private);
            }
            let Some(next) = e.logical.checked_add(e.length) else {
                return Some(private);
            };
            if e.length == 0 || next <= start {
                return Some(private);
            }
            start = next;
        }
    }
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

fn walk_usage(root: &Path, track_rel: bool, mut on_file: impl FnMut(&Path, Usage)) -> Usage {
    use std::os::unix::fs::MetadataExt;
    let mut total = Usage::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = if track_rel {
            root.join(&rel_dir)
        } else if rel_dir.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            rel_dir.clone()
        };
        // Folders can vanish mid-walk (a build or a delete running).
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if track_rel {
                    stack.push(rel_dir.join(entry.file_name()));
                } else {
                    stack.push(entry.path());
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            let p = entry.path();
            let allocated = md.blocks().saturating_mul(512);
            let private = if md.blocks() == 0 {
                0
            } else {
                private_size(&p).unwrap_or(allocated)
            };
            let u = Usage {
                files: 1,
                logical: md.len(),
                allocated,
                private,
            };
            total += u;
            if track_rel {
                let rel = rel_dir.join(entry.file_name());
                on_file(&rel, u);
            }
        }
    }
    total
}

/// Walk `root` without following symlinks and add up its files.
pub fn usage(root: &Path) -> Usage {
    walk_usage(root, false, |_, _| {})
}

/// Like `usage`, plus the usage per path `depth` levels below `root`
/// (`depth` 1 = top-level entries), for finding what grows.
pub fn usage_by(root: &Path, depth: usize) -> (Usage, Vec<(PathBuf, Usage)>) {
    if depth == 0 {
        return (usage(root), Vec::new());
    }
    let mut parts: std::collections::BTreeMap<PathBuf, Usage> = Default::default();
    let total = walk_usage(root, true, |rel, u| {
        let key: PathBuf = rel.components().take(depth).collect();
        *parts.entry(key).or_default() += u;
    });
    (total, parts.into_iter().collect())
}

/// Processes of this user whose current directory is inside `root`
/// (canonical path), as (pid, cwd). Agents and shells sit in their workspace.
#[cfg(target_os = "linux")]
pub fn cwd_users(root: &Path) -> Vec<(u32, PathBuf)> {
    // Other users' `/proc/<pid>/cwd` cannot be read; their processes do
    // not sit in this user's workspaces.
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
            let cwd = std::fs::read_link(e.path().join("cwd")).ok()?;
            cwd.starts_with(root).then_some((pid, cwd))
        })
        .collect()
}

/// Processes of this user whose current directory is inside `root`
/// (canonical path), as (pid, cwd). Agents and shells sit in their workspace.
#[cfg(target_os = "macos")]
pub fn cwd_users(root: &Path) -> Vec<(u32, PathBuf)> {
    let est = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if est <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::c_int; (est as usize).max(1024).saturating_add(256)];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    let mut out = Vec::new();
    for &pid in pids
        .iter()
        .take((n.max(0) as usize).min(pids.len()))
        .filter(|&&p| p > 0)
    {
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
        // SAFETY: `raw` is a valid slice of `c_char` (same size/alignment as `u8`)
        // for at least `len` elements.
        let bytes = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), len) };
        let cwd = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        if cwd.starts_with(root) {
            out.push((pid as u32, cwd));
        }
    }
    out
}

fn relink(link: &Path, target: &Path) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = match link.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".agent-ws-link-{}-{seq}", std::process::id()));
    std::os::unix::fs::symlink(target, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, link) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
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
    fn only_certificates_spares_bundles_not_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let check = |body: &[u8]| {
            let p = tmp.path().join("f.pem");
            std::fs::write(&p, body).unwrap();
            only_certificates(&p)
        };
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        assert!(check(cert.as_bytes()));
        assert!(check(cert.replace('\n', "\r\n").as_bytes()), "CRLF");
        let crl = "-----BEGIN X509 CRL-----\nMIIB\n-----END X509 CRL-----\n";
        assert!(check(format!("# Issuer: CN=x\n{cert}\n{crl}").as_bytes()));
        assert!(
            !check(format!("{cert}  -----BEGIN PRIVATE KEY-----\nMIIE\n").as_bytes()),
            "a key anywhere makes it secret"
        );
        assert!(!check(b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n"));
        assert!(!check(
            b"-----BEGIN PGP MESSAGE-----\nhQEM\n-----END PGP MESSAGE-----\n"
        ));
        assert!(!check(b"key"), "not PEM");
        assert!(!check(b""));
        assert!(!only_certificates(&tmp.path().join("missing.pem")));
        std::fs::create_dir(tmp.path().join("dir.pem")).unwrap();
        assert!(!only_certificates(&tmp.path().join("dir.pem")));
    }

    #[test]
    fn normalize_detects_escapes() {
        assert_eq!(normalize(Path::new("a/../b")), Some(PathBuf::from("b")));
        assert_eq!(normalize(Path::new("a/./b/..")), Some(PathBuf::from("a")));
        assert_eq!(normalize(Path::new("../x")), None);
        assert_eq!(normalize(Path::new("a/../../x")), None);
        assert_eq!(
            normalize_abs(Path::new("/a/b/../c")),
            Some(PathBuf::from("/a/c"))
        );
        assert_eq!(normalize_abs(Path::new("/..")), None);
    }

    #[test]
    fn only_certificates_rejects_mixed_secrets_and_unclosed_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("mix.pem");
        std::fs::write(
            &p,
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\nAPI_KEY=secret\n",
        )
        .unwrap();
        assert!(!only_certificates(&p));

        std::fs::write(&p, "-----BEGIN CERTIFICATE-----\nMIIB\n").unwrap();
        assert!(!only_certificates(&p));

        std::fs::write(
            &p,
            "Bag Attributes\n    friendlyName: Test\nsubject=CN=x\n-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(only_certificates(&p));
    }

    #[test]
    fn scan_blocks_traversal_retarget_and_secret_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("repo");
        let dst = tmp.path().join("ws");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(dst.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "ok").unwrap();
        std::fs::write(dst.join("a.txt"), "ok").unwrap();
        std::fs::write(tmp.path().join("outside"), "secret").unwrap();

        // Absolute symlink with `..` attempting to escape src_root via prefix match.
        std::os::unix::fs::symlink(src.join("sub/../../outside"), dst.join("abs_escape")).unwrap();
        // Relative symlink climbing out of sub and back into src_root.
        std::os::unix::fs::symlink("../../repo/a.txt", dst.join("sub/back_in")).unwrap();
        // Secret symlink (must be deleted even though it is a symlink).
        std::os::unix::fs::symlink(tmp.path().join("outside"), dst.join(".env")).unwrap();

        let tracked = std::collections::HashSet::new();
        let s = scan(&src, &dst, &tracked, &|n| n == ".env").unwrap();
        assert_eq!(s.secrets, [".env"]);
        assert!(!dst.join(".env").exists());
        assert_eq!(s.retargeted, [PathBuf::from("sub/back_in")]);
        assert_eq!(
            std::fs::read_link(dst.join("sub/back_in")).unwrap(),
            dst.join("a.txt")
        );
        assert_eq!(s.external.len(), 1);
        assert_eq!(s.external[0].0, PathBuf::from("abs_escape"));
    }

    #[test]
    fn clone_tree_handles_readonly_dir_with_nested_exclude() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("ro/skip")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("ro/keep.txt"), "k").unwrap();
        std::fs::write(src.join("ro/skip/x.txt"), "x").unwrap();
        std::fs::set_permissions(src.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();

        let calls = clone_tree(&src, &dst, Path::new(""), &[PathBuf::from("./ro/skip")]).unwrap();
        // Restore write bit so tempdir cleanup succeeds cleanly.
        let _ = std::fs::set_permissions(src.join("ro"), std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(dst.join("ro"), std::fs::Permissions::from_mode(0o755));
        assert!(calls >= 1);
        assert!(dst.join("ro/keep.txt").exists());
        assert!(!dst.join("ro/skip").exists());
    }
}
