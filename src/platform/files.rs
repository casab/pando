//! Locks, permission bits, executability, links, file identity and the
//! bytes of a path.

use std::borrow::Cow;
use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Waits for an exclusive lock on `file`, held until the file is closed or
/// the process ends. Advisory: it keeps out whoever asks for it, nothing
/// else.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    imp::lock(file, true).map(|_| ())
}

/// Takes the lock [`lock_exclusive`] waits for, if nobody holds it:
/// `Ok(false)` when somebody does.
pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    imp::lock(file, false)
}

/// The `rwx` bits of the file `meta` describes, where the OS has them.
pub fn permission_bits(meta: &Metadata) -> Option<u32> {
    imp::permission_bits(meta)
}

/// Sets the `rwx` bits of `path`; nothing to do where the OS has none.
pub fn set_permission_bits(path: &Path, bits: u32) -> io::Result<()> {
    imp::set_permission_bits(path, bits)
}

/// A new file at `path` with exactly `bits`, whatever the umask: there is
/// never a moment it is open to more. Fails when `path` exists.
pub fn create_new(path: &Path, bits: u32) -> io::Result<File> {
    imp::create_new(path, bits)
}

/// Whether the user pando runs as owns the file `meta` describes.
pub fn owned_by_current_user(meta: &Metadata) -> bool {
    imp::owned_by_current_user(meta)
}

/// Whether `path` is a file this user could run, following links.
pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && imp::runnable(path, &meta))
}

/// A symbolic link at `link` to `target`.
pub fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    imp::symlink(target, link)
}

/// Which file a path names right now: a file put in another's place behind
/// the same path has another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    device: u64,
    file: u64,
}

impl FileId {
    /// The file `meta` describes; `None` where the OS cannot say from its
    /// metadata.
    pub fn of(meta: &Metadata) -> Option<FileId> {
        imp::file_id(meta)
    }
}

/// The bytes of `path` as the OS stores them, for a digest that must stay
/// the same across versions: a project's id is one.
pub fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    imp::path_bytes(path)
}

#[cfg(unix)]
mod imp {
    use super::FileId;
    use std::borrow::Cow;
    use std::fs::{File, Metadata};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// `flock(2)`; `Ok(false)` when not waiting and somebody holds it.
    pub(super) fn lock(file: &File, wait: bool) -> io::Result<bool> {
        let operation = match wait {
            true => libc::LOCK_EX,
            false => libc::LOCK_EX | libc::LOCK_NB,
        };
        // SAFETY: a descriptor `file` owns, open for the whole call.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        // EWOULDBLOCK and EAGAIN are the same value on every Unix pando
        // builds for.
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) if !wait => Ok(false),
            _ => Err(err),
        }
    }

    pub(super) fn permission_bits(meta: &Metadata) -> Option<u32> {
        Some(meta.permissions().mode() & 0o777)
    }

    pub(super) fn set_permission_bits(path: &Path, bits: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
    }

    pub(super) fn create_new(path: &Path, bits: u32) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(bits)
            .open(path)?;
        // Exactly `bits` whatever the umask made of the mode above.
        file.set_permissions(std::fs::Permissions::from_mode(bits))?;
        Ok(file)
    }

    pub(super) fn owned_by_current_user(meta: &Metadata) -> bool {
        // SAFETY: geteuid takes nothing and cannot fail.
        meta.uid() == unsafe { libc::geteuid() }
    }

    /// Any execute bit: whose it is, the OS decides when the file is run.
    pub(super) fn runnable(_path: &Path, meta: &Metadata) -> bool {
        meta.permissions().mode() & 0o111 != 0
    }

    pub(super) fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    pub(super) fn file_id(meta: &Metadata) -> Option<FileId> {
        Some(FileId {
            device: meta.dev(),
            file: meta.ino(),
        })
    }

    pub(super) fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
        Cow::Borrowed(path.as_os_str().as_bytes())
    }
}
