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
    let md = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
            .with_context(|| format!("symlink {}", dst.display()))?;
        return Ok(());
    }
    if ft.is_file() {
        return reflink(src, dst, &md);
    }
    if !ft.is_dir() {
        return Ok(());
    }

    std::fs::create_dir(dst).with_context(|| format!("mkdir {}", dst.display()))?;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    let mut dirs = vec![(dst.to_path_buf(), md)];
    let mut files: Vec<(PathBuf, PathBuf, std::fs::Metadata)> = Vec::new();
    while let Some((s, d)) = stack.pop() {
        for entry in std::fs::read_dir(&s)? {
            let entry = entry?;
            let child_src = entry.path();
            let child_dst = d.join(entry.file_name());
            let child_md = std::fs::symlink_metadata(&child_src)
                .with_context(|| format!("stat {}", child_src.display()))?;
            let child_ft = child_md.file_type();
            if child_ft.is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&child_src)?, &child_dst)
                    .with_context(|| format!("symlink {}", child_dst.display()))?;
            } else if child_ft.is_dir() {
                std::fs::create_dir(&child_dst)
                    .with_context(|| format!("mkdir {}", child_dst.display()))?;
                stack.push((child_src, child_dst.clone()));
                dirs.push((child_dst, child_md));
            } else if child_ft.is_file() {
                files.push((child_src, child_dst, child_md));
            }
        }
    }

    const BATCH: usize = 16;
    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let workers = max_threads.min(files.len().div_ceil(BATCH));
    if workers > 1 {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let first_err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        if failed.load(Ordering::Relaxed) {
                            break;
                        }
                        let start = next.fetch_add(BATCH, Ordering::Relaxed);
                        if start >= files.len() {
                            break;
                        }
                        let end = (start + BATCH).min(files.len());
                        for (s, d, child_md) in &files[start..end] {
                            if failed.load(Ordering::Relaxed) {
                                return;
                            }
                            if let Err(e) = reflink(s, d, child_md) {
                                failed.store(true, Ordering::Relaxed);
                                let mut guard = first_err.lock().unwrap_or_else(|p| p.into_inner());
                                if guard.is_none() {
                                    *guard = Some(e);
                                }
                                return;
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
            return Err(e);
        }
    } else {
        for (s, d, child_md) in &files {
            reflink(s, d, child_md)?;
        }
    }

    // After the children: a read-only folder could not take them, and
    // creating them changed its mtime. Apply deepest-first.
    for (d, dir_md) in dirs.into_iter().rev() {
        let _ = keep_times(&d, &dir_md);
        std::fs::set_permissions(&d, dir_md.permissions())?;
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

struct Excludes {
    exact: Vec<PathBuf>,
    globs: Vec<String>,
}

fn parse_excludes(excludes: &[PathBuf]) -> Excludes {
    let mut parsed = Excludes {
        exact: Vec::new(),
        globs: Vec::new(),
    };
    for e in excludes {
        let Some(norm) = normalize(e) else {
            continue;
        };
        if norm.as_os_str().is_empty() {
            continue;
        }
        let s = norm.to_string_lossy();
        if s.contains('*') || s.contains('?') {
            parsed.globs.push(s.into_owned());
        } else {
            parsed.exact.push(norm);
        }
    }
    parsed
}

/// Clone the children of `src` into the existing directory `dst`, skipping
/// `excludes` (exact relative paths or glob patterns like `**/node_modules`,
/// `apps/*/dist`, `*.log`). A directory that can contain an excluded path is
/// recreated and its other children cloned one by one; unaffected directories
/// are cloned whole in a single `clonefile` call on macOS.
pub fn clone_tree(
    src_root: &Path,
    dst_root: &Path,
    rel: &Path,
    excludes: &[PathBuf],
) -> Result<usize> {
    let parsed = parse_excludes(excludes);
    clone_tree_norm(src_root, dst_root, rel, &parsed)
}

fn clone_tree_norm(
    src_root: &Path,
    dst_root: &Path,
    rel: &Path,
    excludes: &Excludes,
) -> Result<usize> {
    let mut calls = 0;
    let dir = src_root.join(rel);
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let r = rel.join(entry.file_name());
        if excludes.exact.iter().any(|e| e == &r)
            || excludes.globs.iter().any(|g| match_path_glob(g, &r))
        {
            continue;
        }
        let ft = entry.file_type()?;
        let has_nested_exact = excludes.exact.iter().any(|e| e.starts_with(&r) && e != &r);
        let has_nested_glob = excludes.globs.iter().any(|g| may_match_under(g, &r));
        if ft.is_dir() && (has_nested_exact || has_nested_glob) {
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

fn scan_entry(
    src_cmp: &Path,
    dst_root: &Path,
    rel_dir: &Path,
    entry: &std::fs::DirEntry,
    tracked: &std::collections::HashSet<String>,
    is_secret: &dyn Fn(&str) -> bool,
    found: &mut Scan,
) -> Result<Option<PathBuf>> {
    let name = entry.file_name();
    // The worktree's `.git` link and submodule git dirs are Git's own
    // data, never workspace content.
    if name == ".git" {
        return Ok(None);
    }
    let rel = rel_dir.join(&name);
    let ft = entry.file_type()?;
    if ft.is_dir() {
        return Ok(Some(rel));
    }
    let p = entry.path();
    if ft.is_symlink() {
        if is_secret(&name.to_string_lossy()) && !tracked.contains(&*rel.to_string_lossy()) {
            std::fs::remove_file(&p)?;
            found.secrets.push(rel.to_string_lossy().into_owned());
            return Ok(None);
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
            if let Some(meant) = normalize_abs(&src_cmp.join(rel_dir).join(&target)) {
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
    Ok(None)
}

fn dir_skipped(rel: &Path, skip: &[String]) -> bool {
    skip.iter().any(|pat| match_path_glob(pat, rel))
}

fn scan_subtree(
    src_cmp: &Path,
    dst_root: &Path,
    start_rel: PathBuf,
    tracked: &std::collections::HashSet<String>,
    is_secret: &dyn Fn(&str) -> bool,
    skip: &[String],
) -> Result<Scan> {
    let mut found = Scan::default();
    let mut stack = vec![start_rel];
    while let Some(rel_dir) = stack.pop() {
        for entry in std::fs::read_dir(dst_root.join(&rel_dir))? {
            let entry = entry?;
            if let Some(sub_rel) = scan_entry(
                src_cmp, dst_root, &rel_dir, &entry, tracked, is_secret, &mut found,
            )? && !dir_skipped(&sub_rel, skip)
            {
                stack.push(sub_rel);
            }
        }
    }
    Ok(found)
}

fn merge_scan(dst: &mut Scan, part: Scan) {
    dst.retargeted.extend(part.retargeted);
    dst.external.extend(part.external);
    dst.secrets.extend(part.secrets);
}

fn sort_scan(found: &mut Scan) {
    found.retargeted.sort();
    found.external.sort();
    found.secrets.sort();
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
    let mut found = scan_subtree(src_cmp, dst_root, PathBuf::new(), tracked, is_secret, &[])?;
    sort_scan(&mut found);
    Ok(found)
}

/// Parallel version of `scan` that skips subdirectories matching `skip` via
/// `match_path_glob`. Top-level subdirectories inside `dst_root` are scanned
/// in parallel with `std::thread::scope` when at least two remain.
pub fn scan_with_skip(
    src_root: &Path,
    dst_root: &Path,
    tracked: &std::collections::HashSet<String>,
    is_secret: &(dyn Fn(&str) -> bool + Sync),
    skip: &[String],
) -> Result<Scan> {
    let norm_src = normalize_abs(src_root);
    let src_cmp = norm_src.as_deref().unwrap_or(src_root);
    let mut found = Scan::default();
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dst_root)? {
        let entry = entry?;
        if let Some(sub_rel) = scan_entry(
            src_cmp,
            dst_root,
            Path::new(""),
            &entry,
            tracked,
            is_secret,
            &mut found,
        )? && !dir_skipped(&sub_rel, skip)
        {
            subdirs.push(sub_rel);
        }
    }

    if subdirs.len() >= 2 {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(subdirs.len())
            .max(1);
        if workers > 1 {
            let next = AtomicUsize::new(0);
            let failed = AtomicBool::new(false);
            let results: Vec<Result<Scan>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..workers)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut local = Scan::default();
                            loop {
                                if failed.load(Ordering::Relaxed) {
                                    break;
                                }
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                if i >= subdirs.len() {
                                    break;
                                }
                                match scan_subtree(
                                    src_cmp,
                                    dst_root,
                                    subdirs[i].clone(),
                                    tracked,
                                    is_secret,
                                    skip,
                                ) {
                                    Ok(part) => merge_scan(&mut local, part),
                                    Err(e) => {
                                        failed.store(true, Ordering::Relaxed);
                                        return Err(e);
                                    }
                                }
                            }
                            Ok(local)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("scan worker panicked"))
                    .collect()
            });
            for res in results {
                merge_scan(&mut found, res?);
            }
        } else {
            for sub in subdirs {
                merge_scan(
                    &mut found,
                    scan_subtree(src_cmp, dst_root, sub, tracked, is_secret, skip)?,
                );
            }
        }
    } else if let Some(sub) = subdirs.pop() {
        merge_scan(
            &mut found,
            scan_subtree(src_cmp, dst_root, sub, tracked, is_secret, skip)?,
        );
    }

    sort_scan(&mut found);
    Ok(found)
}

/// A shared-extent sample for cohort / orphan disk accounting across workspaces.
#[derive(Default, Clone, Copy, Debug)]
pub struct ExtentSample {
    pub id: u64,
    pub bytes: u64,
}

/// `ATTR_CMNEXT_PRIVATESIZE` from `<sys/attr.h>` (not in the libc crate).
#[cfg(target_os = "macos")]
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
/// `ATTR_CMNEXT_CLONEID` from `<sys/attr.h>` (not in the libc crate).
#[cfg(target_os = "macos")]
const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;

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

/// Query APFS `privatesize` and `cloneid` in a single `getattrlist` syscall.
#[cfg(target_os = "macos")]
fn private_and_clone_id(path: &Path) -> Option<(u64, u64)> {
    let c = cstr(path).ok()?;
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.forkattr = ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID;
    // u32 length, off_t privatesize, u64 cloneid.
    let mut buf = [0u8; 24];
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if rc != 0 || u32::from_ne_bytes(buf[..4].try_into().ok()?) < 20 {
        return None;
    }
    let private = i64::from_ne_bytes(buf[4..12].try_into().ok()?).max(0) as u64;
    let clone_id = u64::from_ne_bytes(buf[12..20].try_into().ok()?);
    Some((private, clone_id))
}

#[cfg(target_os = "linux")]
fn hash_extent(physical: u64, length: u64) -> u64 {
    let mut x = physical ^ length.rotate_left(32).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(target_os = "linux")]
fn fiemap_walk(path: &Path, collect_shared: bool) -> Option<(u64, Vec<ExtentSample>)> {
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
    let mut samples = Vec::new();
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
            return Some((private, samples));
        }
        for e in &map.extents[..(map.mapped as usize).min(64)] {
            if e.flags & FIEMAP_EXTENT_SHARED == 0 {
                private += e.length;
            } else if collect_shared {
                samples.push(ExtentSample {
                    id: hash_extent(e.physical, e.length),
                    bytes: e.length,
                });
            }
            if e.flags & FIEMAP_EXTENT_LAST != 0 {
                return Some((private, samples));
            }
            let Some(next) = e.logical.checked_add(e.length) else {
                return Some((private, samples));
            };
            if e.length == 0 || next <= start {
                return Some((private, samples));
            }
            start = next;
        }
    }
}

/// Bytes of a file's data that no other file shares: the extents `FIEMAP`
/// does not flag as shared (Btrfs and XFS flag reflinked ones). `None` if
/// the filesystem cannot say.
#[cfg(target_os = "linux")]
pub fn private_size(path: &Path) -> Option<u64> {
    fiemap_walk(path, false).map(|(private, _)| private)
}

#[cfg(target_os = "linux")]
fn private_and_shared_extents(path: &Path) -> Option<(u64, Vec<ExtentSample>)> {
    fiemap_walk(path, true)
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

fn file_private_and_shared(p: &Path, allocated: u64, samples: &mut Vec<ExtentSample>) -> u64 {
    #[cfg(target_os = "macos")]
    if let Some((private, clone_id)) = private_and_clone_id(p) {
        if allocated > private && clone_id != 0 {
            samples.push(ExtentSample {
                id: clone_id,
                bytes: allocated - private,
            });
        }
        return private;
    }
    #[cfg(target_os = "linux")]
    if let Some((private, file_samples)) = private_and_shared_extents(p) {
        samples.extend(file_samples);
        return private;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = samples;
    private_size(p).unwrap_or(allocated)
}

/// Walk `root` without following symlinks, returning total usage, optional
/// per-entry usage at `depth` levels below `root`, and (when `collect_shared`
/// is true) shared extent / clone samples for cohort disk accounting.
pub fn usage_detailed(
    root: &Path,
    depth: usize,
    collect_shared: bool,
) -> (Usage, Vec<(PathBuf, Usage)>, Vec<ExtentSample>) {
    use std::os::unix::fs::MetadataExt;
    let track_rel = depth > 0;
    let mut total = Usage::default();
    let mut parts: std::collections::BTreeMap<PathBuf, Usage> = Default::default();
    let mut samples = Vec::new();
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
            } else if collect_shared {
                file_private_and_shared(&p, allocated, &mut samples)
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
                let key: PathBuf = rel.components().take(depth).collect();
                *parts.entry(key).or_default() += u;
            }
        }
    }
    (total, parts.into_iter().collect(), samples)
}

/// Walk `root` without following symlinks and add up its files.
pub fn usage(root: &Path) -> Usage {
    usage_detailed(root, 0, false).0
}

/// Like `usage`, plus the usage per path `depth` levels below `root`
/// (`depth` 1 = top-level entries), for finding what grows.
pub fn usage_by(root: &Path, depth: usize) -> (Usage, Vec<(PathBuf, Usage)>) {
    let (total, parts, _) = usage_detailed(root, depth, false);
    (total, parts)
}

fn collect_file_shared_ids(path: &Path, ids: &mut std::collections::HashSet<u64>) {
    #[cfg(target_os = "macos")]
    if let Some((_, clone_id)) = private_and_clone_id(path)
        && clone_id != 0
    {
        ids.insert(clone_id);
    }
    #[cfg(target_os = "linux")]
    if let Some((_, samples)) = private_and_shared_extents(path) {
        for s in samples {
            ids.insert(s.id);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = (path, ids);
}

fn collect_dir_shared_ids(dir: &Path, ids: &mut std::collections::HashSet<u64>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                collect_file_shared_ids(&entry.path(), ids);
            }
        }
    }
}

/// Collect all clone IDs (macOS) or shared extent IDs (Linux) present in
/// `root`, skipping `.git` and parallelizing across top-level subdirectories.
pub fn shared_ids(root: &Path) -> std::collections::HashSet<u64> {
    let mut ids = std::collections::HashSet::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return ids;
    };
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        if entry.file_name() == ".git" {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else if ft.is_file() {
            collect_file_shared_ids(&entry.path(), &mut ids);
        }
    }

    if subdirs.len() >= 2 {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(subdirs.len())
            .max(1);
        if workers > 1 {
            let next = AtomicUsize::new(0);
            let local_sets = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..workers)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut local = std::collections::HashSet::new();
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                if i >= subdirs.len() {
                                    break;
                                }
                                collect_dir_shared_ids(&subdirs[i], &mut local);
                            }
                            local
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("shared_ids worker panicked"))
                    .collect::<Vec<_>>()
            });
            for set in local_sets {
                ids.extend(set);
            }
        } else {
            for d in subdirs {
                collect_dir_shared_ids(&d, &mut ids);
            }
        }
    } else if let Some(d) = subdirs.pop() {
        collect_dir_shared_ids(&d, &mut ids);
    }
    ids
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

fn pattern_segments(pattern: &str) -> Option<Vec<&str>> {
    let mut segs = Vec::new();
    for s in pattern.split('/') {
        if s == ".." {
            return None;
        }
        if !s.is_empty() && s != "." {
            segs.push(s);
        }
    }
    Some(segs)
}

fn path_segments(p: &Path) -> Option<Vec<String>> {
    let norm = normalize(p)?;
    Some(
        norm.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect(),
    )
}

/// Match a relative path against a path glob pattern supporting `*` (within
/// segment), `?` (within segment), and `**` (zero or more path segments).
pub fn match_path_glob(pattern: &str, rel: &Path) -> bool {
    let Some(pat) = pattern_segments(pattern) else {
        return false;
    };
    let Some(path) = path_segments(rel) else {
        return false;
    };
    if pat.is_empty() || path.is_empty() {
        return false;
    }
    let (pn, sn) = (pat.len(), path.len());
    let mut next = vec![false; sn + 1];
    next[sn] = true;
    for i in (0..pn).rev() {
        let mut cur = vec![false; sn + 1];
        if pat[i] == "**" {
            cur[sn] = next[sn];
            for j in (0..sn).rev() {
                cur[j] = next[j] || cur[j + 1];
            }
        } else {
            for j in 0..sn {
                cur[j] = next[j + 1] && glob(pat[i], &path[j]);
            }
        }
        next = cur;
    }
    next[0]
}

/// Returns true only if a strict descendant under `dir_rel` could match
/// `pattern` (so `clone_tree` can still O(1)-clone `dir_rel` when false).
pub fn may_match_under(pattern: &str, dir_rel: &Path) -> bool {
    let Some(pat) = pattern_segments(pattern) else {
        return false;
    };
    let Some(dir) = path_segments(dir_rel) else {
        return false;
    };
    if pat.is_empty() {
        return false;
    }
    let (pn, dn) = (pat.len(), dir.len());
    let mut next = vec![false; dn + 1];
    for i in (0..pn).rev() {
        let mut cur = vec![false; dn + 1];
        cur[dn] = true;
        if pat[i] == "**" {
            for j in (0..dn).rev() {
                cur[j] = next[j] || cur[j + 1];
            }
        } else {
            for j in 0..dn {
                cur[j] = next[j + 1] && glob(pat[i], &dir[j]);
            }
        }
        next = cur;
    }
    next[0]
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

    #[test]
    fn match_path_glob_and_may_match_under_behave_accurately() {
        assert!(match_path_glob("target", Path::new("target")));
        assert!(!match_path_glob("target", Path::new("sub/target")));
        assert!(match_path_glob("*.log", Path::new("app.log")));
        assert!(!match_path_glob("*.log", Path::new("sub/app.log")));
        assert!(match_path_glob(
            "**/node_modules",
            Path::new("node_modules")
        ));
        assert!(match_path_glob(
            "**/node_modules",
            Path::new("apps/web/node_modules")
        ));
        assert!(!match_path_glob(
            "**/node_modules",
            Path::new("apps/node_modules/pkg")
        ));
        assert!(match_path_glob("apps/*/dist", Path::new("apps/web/dist")));
        assert!(!match_path_glob(
            "apps/*/dist",
            Path::new("apps/web/sub/dist")
        ));
        assert!(match_path_glob("a/**/b", Path::new("a/b")));
        assert!(match_path_glob("a/**/b", Path::new("a/x/y/b")));
        assert!(match_path_glob("apps/a?/out", Path::new("apps/a1/out")));
        assert!(!match_path_glob("apps/a?/out", Path::new("apps/a12/out")));

        assert!(may_match_under("**/node_modules", Path::new("")));
        assert!(may_match_under("**/node_modules", Path::new("apps/web")));
        assert!(may_match_under("apps/*/dist", Path::new("apps")));
        assert!(may_match_under("apps/*/dist", Path::new("apps/web")));
        assert!(!may_match_under("apps/*/dist", Path::new("apps/web/src")));
        assert!(!may_match_under("apps/*/dist", Path::new("packages")));
        assert!(!may_match_under("*.log", Path::new("src")));
        assert!(!may_match_under("target", Path::new("target")));
        assert!(may_match_under("a/**/b", Path::new("a/b")));
        assert!(!may_match_under("a/**/b", Path::new("c")));
    }

    #[test]
    fn clone_tree_supports_globs_and_preserves_o1_clones() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("apps/web/dist")).unwrap();
        std::fs::create_dir_all(src.join("apps/web/src/deep")).unwrap();
        std::fs::create_dir_all(src.join("apps/api/node_modules/pkg")).unwrap();
        std::fs::create_dir_all(src.join("packages/core/lib")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        std::fs::write(src.join("root.log"), "drop").unwrap();
        std::fs::write(src.join("keep.txt"), "ok").unwrap();
        std::fs::write(src.join("apps/web/dist/bundle.js"), "drop").unwrap();
        std::fs::write(src.join("apps/web/src/deep/main.ts"), "ok").unwrap();
        std::fs::write(src.join("apps/api/node_modules/pkg/index.js"), "drop").unwrap();
        std::fs::write(src.join("packages/core/lib/index.ts"), "ok").unwrap();

        // Exclude `apps/*/dist`, `*.log`, and `packages/missing` (none under packages/core).
        let dst_no_star = tmp.path().join("dst_no_star");
        std::fs::create_dir_all(&dst_no_star).unwrap();
        let calls_no_star = clone_tree(
            &src,
            &dst_no_star,
            Path::new(""),
            &[
                PathBuf::from("apps/*/dist"),
                PathBuf::from("*.log"),
                PathBuf::from("apps/api/node_modules"),
            ],
        )
        .unwrap();
        // `packages` has no matching exact or glob exclude under it, so it is cloned in 1 call;
        // `apps/web/src` is likewise cloned in 1 call; `keep.txt` in 1 call -> small call count.
        assert!(dst_no_star.join("keep.txt").exists());
        assert!(!dst_no_star.join("root.log").exists());
        assert!(!dst_no_star.join("apps/web/dist").exists());
        assert!(dst_no_star.join("apps/web/src/deep/main.ts").exists());
        assert!(!dst_no_star.join("apps/api/node_modules").exists());
        assert!(dst_no_star.join("packages/core/lib/index.ts").exists());
        assert_eq!(calls_no_star, 3);

        // Now test `**/node_modules` alongside `apps/*/dist` and `*.log`.
        let calls = clone_tree(
            &src,
            &dst,
            Path::new(""),
            &[
                PathBuf::from("**/node_modules"),
                PathBuf::from("apps/*/dist"),
                PathBuf::from("*.log"),
            ],
        )
        .unwrap();
        assert!(calls >= 3);
        assert!(dst.join("keep.txt").exists());
        assert!(!dst.join("root.log").exists());
        assert!(!dst.join("apps/web/dist").exists());
        assert!(dst.join("apps/web/src/deep/main.ts").exists());
        assert!(!dst.join("apps/api/node_modules").exists());
        assert!(dst.join("packages/core/lib/index.ts").exists());
    }

    #[test]
    fn scan_with_skip_parallelizes_and_skips_matching_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("a/node_modules")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::create_dir_all(dst.join("a/node_modules")).unwrap();
        std::fs::create_dir_all(dst.join("b")).unwrap();
        std::fs::create_dir_all(dst.join("c")).unwrap();

        std::fs::write(src.join("shared.txt"), "ok").unwrap();
        std::fs::write(dst.join("shared.txt"), "ok").unwrap();
        std::fs::write(dst.join(".env"), "root-secret").unwrap();
        std::fs::write(dst.join("a/.env"), "a-secret").unwrap();
        // Inside skipped `**/node_modules`, this `.env` must remain untouched.
        std::fs::write(dst.join("a/node_modules/.env"), "ignored").unwrap();
        std::fs::write(dst.join("b/.env"), "b-secret").unwrap();
        std::os::unix::fs::symlink(src.join("shared.txt"), dst.join("c/link")).unwrap();
        std::os::unix::fs::symlink("/outside/path", dst.join("b/ext")).unwrap();

        let tracked = std::collections::HashSet::new();
        let skip = vec!["**/node_modules".to_string()];
        let s = scan_with_skip(&src, &dst, &tracked, &|n| n == ".env", &skip).unwrap();
        assert_eq!(s.secrets, [".env", "a/.env", "b/.env"]);
        assert!(dst.join("a/node_modules/.env").exists());
        assert_eq!(s.retargeted, [PathBuf::from("c/link")]);
        assert_eq!(
            std::fs::read_link(dst.join("c/link")).unwrap(),
            dst.join("shared.txt")
        );
        assert_eq!(
            s.external,
            [(PathBuf::from("b/ext"), PathBuf::from("/outside/path"))]
        );
    }

    #[test]
    fn usage_detailed_and_shared_ids_track_depth_and_clones() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(src.join(".git")).unwrap();
        write_synced(&src.join("top.bin"), &vec![1u8; 1 << 20]);
        write_synced(&src.join("sub/nested.bin"), &vec![2u8; 1 << 20]);
        write_synced(&src.join(".git/packed"), &vec![3u8; 1 << 16]);

        let ws = tmp.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        clone_tree(&src, &ws, Path::new(""), &[PathBuf::from(".git")]).unwrap();

        let (total, by_depth1, samples_off) = usage_detailed(&ws, 1, false);
        assert!(samples_off.is_empty());
        assert_eq!(total.files, 2);
        assert_eq!(total.logical, 2 << 20);
        assert_eq!(by_depth1.len(), 2);
        assert_eq!(by_depth1[0].0, PathBuf::from("sub"));
        assert_eq!(by_depth1[0].1.files, 1);
        assert_eq!(by_depth1[1].0, PathBuf::from("top.bin"));
        assert_eq!(by_depth1[1].1.files, 1);

        let (total_d, _, samples_on) = usage_detailed(&ws, 1, true);
        assert_eq!(total_d.files, 2);
        assert_eq!(total_d.private, 0);
        assert_eq!(samples_on.len(), 2);

        let src_ids = shared_ids(&src);
        for sample in &samples_on {
            assert_ne!(sample.id, 0);
            assert!(sample.bytes >= 1 << 20);
            assert!(
                src_ids.contains(&sample.id),
                "source shared_ids should contain clone id {}",
                sample.id
            );
        }

        // Rewriting one cloned file drops its shared sample and makes it private.
        write_synced(&ws.join("top.bin"), &vec![9u8; 1 << 20]);
        let (after, _, samples_after) = usage_detailed(&ws, 1, true);
        assert!(after.private >= 1 << 20);
        assert_eq!(samples_after.len(), 1);
    }
}
