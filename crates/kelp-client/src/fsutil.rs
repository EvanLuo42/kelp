//! File system helpers shared by workspace operations.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use kelp_core::{ContentId, Hash};
use kelp_workspace::HaveEntry;

use crate::Result;

/// Returns the modification time of `path` in nanoseconds since the Unix epoch.
pub(crate) fn mtime(path: &Path) -> Result<u64> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64))
}

/// Returns the content id of the file at `path`.
pub(crate) fn content_of(path: &Path) -> Result<ContentId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(fs::File::open(path)?)?;
    Ok(ContentId(Hash(*hasher.finalize().as_bytes())))
}

/// Returns `true` if the file at `path` still has the content of `have`.
pub(crate) fn file_matches(path: &Path, have: &HaveEntry) -> Result<bool> {
    let meta = match fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if meta.len() != have.size {
        return Ok(false);
    }
    if mtime(path)? == have.mtime {
        return Ok(true);
    }
    Ok(content_of(path)? == have.content)
}

/// Returns `true` if the file at `path`, whose metadata is `entry`, still has
/// the content of `have`.
pub(crate) fn entry_matches(
    path: &Path,
    have: &HaveEntry,
    entry: crate::index::DiskEntry,
) -> Result<bool> {
    if entry.size != have.size {
        return Ok(false);
    }
    if entry.mtime == have.mtime {
        return Ok(true);
    }
    Ok(content_of(path)? == have.content)
}

#[cfg(unix)]
pub(crate) fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(crate) fn is_executable(_path: &Path) -> Result<bool> {
    Ok(false)
}

/// Sets or clears the executable bits of `path`.
#[cfg(unix)]
pub(crate) fn set_executable(path: &Path, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    let mode = if executable {
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn set_executable(_path: &Path, _executable: bool) -> std::io::Result<()> {
    Ok(())
}

/// Makes `path` read-only, or writable by its owner. Missing files are ignored.
pub(crate) fn set_read_only(path: &Path, read_only: bool) -> std::io::Result<()> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        let mode = if read_only {
            mode & !0o222
        } else {
            mode | 0o200
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(read_only);
        fs::set_permissions(path, perms)
    }
}

/// Writes all of `buf` to `file` at `offset`, leaving the file position
/// unspecified so that several threads can write one file.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Writes all of `buf` to `file` at `offset`, leaving the file position
/// unspecified so that several threads can write one file.
#[cfg(windows)]
pub(crate) fn write_all_at(
    file: &fs::File,
    mut buf: &[u8],
    mut offset: u64,
) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Returns `true` if `path` exists and is read-only for its owner.
pub fn is_read_only(path: &Path) -> std::io::Result<bool> {
    Ok(fs::metadata(path)?.permissions().readonly())
}

/// Removes a file and then any directories left empty, up to `root`.
pub(crate) fn remove_file(path: &Path, root: &Path) -> Result<()> {
    set_read_only(path, false)?;
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root)) {
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}
