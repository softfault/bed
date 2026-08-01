//! Recoverable file replacement for bed documents.
//!
//! Data is written and synchronized in the destination directory before the
//! destination name is replaced. Unix-like targets use `rename(2)` through the
//! standard library. Windows uses `ReplaceFileW` for existing files so their
//! metadata is retained, and `MoveFileExW` when creating a new destination.
//!
//! Authoritative reference:
//! - Microsoft Learn [`ReplaceFileW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
//!   and [`MoveFileExW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

/// Replaces `path` only after all `bytes` have been written to a sibling file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = resolve_target(path)?;
    let metadata = match fs::metadata(&target) {
        Ok(metadata) => {
            // Opening without truncation preserves the existing contents while
            // enforcing the destination's write permissions before replacement.
            drop(
                OpenOptions::new()
                    .write(true)
                    .open(&target)
                    .with_context(|| format!("destination is not writable: {}", path.display()))?,
            );
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect destination {}", path.display()));
        }
    };
    let mut temporary = TemporaryFile::create(&target)?;

    if let Some(metadata) = metadata {
        fs::set_permissions(temporary.path(), metadata.permissions())
            .with_context(|| format!("failed to copy permissions from {}", target.display()))?;
    }

    temporary
        .file
        .as_mut()
        .expect("temporary file is open")
        .write_all(bytes)
        .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
    temporary
        .file
        .as_mut()
        .expect("temporary file is open")
        .sync_all()
        .with_context(|| {
            format!(
                "failed to synchronize temporary file for {}",
                path.display()
            )
        })?;
    temporary.replace(&target)?;
    Ok(())
}

fn resolve_target(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .with_context(|| format!("failed to resolve symbolic link {}", path.display())),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect destination {}", path.display()))
        }
    }
}

struct TemporaryFile {
    path: PathBuf,
    file: Option<File>,
    replaced: bool,
}

impl TemporaryFile {
    fn create(target: &Path) -> Result<Self> {
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        let file_name = target
            .file_name()
            .context("document path has no file name")?;

        for _ in 0..100 {
            let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let mut name = OsString::from(".");
            name.push(file_name);
            name.push(format!(".bed.{}.{id}.tmp", std::process::id()));
            let path = parent.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        replaced: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create temporary file beside {}",
                            target.display()
                        )
                    });
                }
            }
        }

        anyhow::bail!(
            "failed to allocate a temporary file name beside {}",
            target.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn replace(&mut self, target: &Path) -> Result<()> {
        drop(self.file.take());
        replace_file(&self.path, target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
        self.replaced = true;
        Ok(())
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.replaced {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    // Flags and errors from winbase.h and winerror.h. ReplaceFileW keeps the
    // destination's ACL and other documented metadata when it already exists.
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        #[link_name = "ReplaceFileW"]
        fn replace_file_ffi(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
        #[link_name = "MoveFileExW"]
        fn move_file_ex(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are terminated UTF-16 buffers that remain alive for the
    // call. Null optional arguments and zero flags are accepted by ReplaceFileW.
    let replaced = unsafe {
        replace_file_ffi(
            target.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced != 0 {
        return Ok(());
    }

    let replace_error = std::io::Error::last_os_error();
    if !matches!(
        replace_error.raw_os_error(),
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
    ) {
        return Err(replace_error);
    }

    // SAFETY: the same path buffers remain valid, and both flags are documented
    // MoveFileExW values. REPLACE_EXISTING also closes a race with file creation.
    let moved = unsafe {
        move_file_ex(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::atomic_write;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEST_FILE: AtomicUsize = AtomicUsize::new(0);

    fn test_path() -> PathBuf {
        let id = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bed-file-{}-{id}.txt", std::process::id()))
    }

    #[test]
    fn creates_and_replaces_files() {
        let path = test_path();
        let _ = fs::remove_file(&path);

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preserves_permissions_when_replacing_a_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_path();
        fs::write(&path, b"first").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        atomic_write(&path, b"second").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_symbolic_link_without_replacing_it() {
        use std::os::unix::fs::symlink;

        let target = test_path();
        let link = target.with_extension("link");
        fs::write(&target, b"first").unwrap();
        symlink(&target, &link).unwrap();

        atomic_write(&link, b"second").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"second");
        fs::remove_file(link).unwrap();
        fs::remove_file(target).unwrap();
    }
}
