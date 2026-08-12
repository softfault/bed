//! Recoverable file replacement for bed documents.
//!
//! Data is written and synchronized in the destination directory before the
//! destination name is replaced. Conditional writes atomically capture the
//! predecessor at commit time and compare that exact version before accepting
//! the write, closing the check/replace race.
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
    let (target, mut temporary) = prepare_write(path, bytes)?;
    temporary.replace(&target)?;
    Ok(())
}

/// Writes only if the destination still contains `expected`.
///
/// `None` means that the destination must still be absent. A conflicting
/// destination is preserved and reported without accepting the replacement.
pub fn atomic_write_if_unchanged(path: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> Result<()> {
    let (target, mut temporary) = prepare_write(path, bytes)?;
    let committed = match expected {
        Some(expected) => temporary.replace_if_unchanged(&target, expected, bytes)?,
        None => temporary.install_if_missing(&target)?,
    };
    anyhow::ensure!(
        committed,
        "{} changed on disk while it was being saved; use :w! to overwrite it",
        path.display()
    );
    Ok(())
}

fn prepare_write(path: &Path, bytes: &[u8]) -> Result<(PathBuf, TemporaryFile)> {
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
    Ok((target, temporary))
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

    fn install_if_missing(&mut self, target: &Path) -> Result<bool> {
        drop(self.file.take());
        match install_file_if_missing(&self.path, target) {
            Ok(()) => {
                self.replaced = true;
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) && fs::symlink_metadata(target).is_ok() =>
            {
                Ok(false)
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to create {}", target.display()))
            }
        }
    }

    fn replace_if_unchanged(
        &mut self,
        target: &Path,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<bool> {
        drop(self.file.take());
        let mut temporary_consumed = false;
        let outcome = conditional_replace(
            &self.path,
            target,
            expected,
            replacement,
            &mut temporary_consumed,
        );
        self.replaced = temporary_consumed;
        let outcome = outcome?;
        Ok(outcome.committed)
    }
}

struct ConditionalReplace {
    committed: bool,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.replaced {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(not(windows))]
fn sync_parent(target: &Path) -> std::io::Result<()> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(windows))]
fn install_file_if_missing(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::hard_link(source, target)?;
    fs::remove_file(source)?;
    sync_parent(target)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)?;
    sync_parent(target)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn conditional_replace(
    source: &Path,
    target: &Path,
    expected: &[u8],
    replacement: &[u8],
    temporary_consumed: &mut bool,
) -> Result<ConditionalReplace> {
    match exchange_files(source, target) {
        Ok(()) => *temporary_consumed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConditionalReplace { committed: false });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to exchange {}", target.display()));
        }
    }

    let predecessor = fs::read(source).with_context(|| {
        format!(
            "failed to verify the captured predecessor of {}; it remains at {}",
            target.display(),
            source.display()
        )
    })?;
    if predecessor == expected {
        fs::remove_file(source).with_context(|| {
            format!(
                "write committed, but failed to remove predecessor {}",
                source.display()
            )
        })?;
        sync_parent(target)?;
        return Ok(ConditionalReplace { committed: true });
    }

    if let Err(error) = exchange_files(source, target) {
        anyhow::bail!(
            "{} changed during save and rollback failed: {error}; the external predecessor remains at {}",
            target.display(),
            source.display()
        );
    }
    let displaced = fs::read(source).with_context(|| {
        format!(
            "failed to verify rollback of {}; displaced data remains at {}",
            target.display(),
            source.display()
        )
    })?;
    if displaced != replacement {
        sync_parent(target)?;
        anyhow::bail!(
            "{} changed more than once during save; the additional version remains at {}",
            target.display(),
            source.display()
        );
    }
    fs::remove_file(source)
        .with_context(|| format!("failed to clean rollback file {}", source.display()))?;
    sync_parent(target)?;
    Ok(ConditionalReplace { committed: false })
}

#[cfg(target_os = "linux")]
fn exchange_files(first: &Path, second: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    const AT_FDCWD: i32 = -100;
    const RENAME_EXCHANGE: u32 = 1 << 1;

    unsafe extern "C" {
        fn renameat2(
            old_directory: i32,
            old_path: *const std::ffi::c_char,
            new_directory: i32,
            new_path: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    let first = CString::new(first.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let second = CString::new(second.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both paths are live NUL-terminated byte strings, AT_FDCWD is a
    // valid directory selector, and RENAME_EXCHANGE requires no pointers out.
    let status = unsafe {
        renameat2(
            AT_FDCWD,
            first.as_ptr(),
            AT_FDCWD,
            second.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn exchange_files(first: &Path, second: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    const RENAME_SWAP: u32 = 0x0000_0002;

    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    let first = CString::new(first.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let second = CString::new(second.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both paths are live NUL-terminated byte strings and RENAME_SWAP
    // is a documented renamex_np flag.
    let status = unsafe { renamex_np(first.as_ptr(), second.as_ptr(), RENAME_SWAP) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "freebsd", target_os = "netbsd"))]
fn conditional_replace(
    source: &Path,
    target: &Path,
    expected: &[u8],
    _replacement: &[u8],
    temporary_consumed: &mut bool,
) -> Result<ConditionalReplace> {
    let mut predecessor = TemporaryFile::create(target)?;
    drop(predecessor.file.take());
    match fs::rename(target, predecessor.path()) {
        Ok(()) => predecessor.replaced = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConditionalReplace { committed: false });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to capture {}", target.display()));
        }
    }

    let captured = fs::read(predecessor.path()).with_context(|| {
        format!(
            "failed to verify {}; its predecessor remains at {}",
            target.display(),
            predecessor.path().display()
        )
    })?;
    if captured != expected {
        match fs::hard_link(predecessor.path(), target) {
            Ok(()) => {
                fs::remove_file(predecessor.path())?;
                sync_parent(target)?;
                return Ok(ConditionalReplace { committed: false });
            }
            Err(error) => anyhow::bail!(
                "{} changed during save and could not be restored: {error}; its predecessor remains at {}",
                target.display(),
                predecessor.path().display()
            ),
        }
    }

    match install_file_if_missing(source, target) {
        Ok(()) => {
            *temporary_consumed = true;
            fs::remove_file(predecessor.path())?;
            sync_parent(target)?;
            Ok(ConditionalReplace { committed: true })
        }
        Err(_error) if fs::symlink_metadata(target).is_ok() => {
            fs::remove_file(predecessor.path())?;
            sync_parent(target)?;
            Ok(ConditionalReplace { committed: false })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to install {}; its predecessor remains at {}",
                target.display(),
                predecessor.path().display()
            )
        }),
    }
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

    #[link(name = "kernel32")]
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
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND)
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

#[cfg(windows)]
fn install_file_if_missing(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are terminated UTF-16 buffers that remain live for
    // the call. Omitting REPLACE_EXISTING makes destination creation atomic.
    let moved = unsafe { move_file_ex(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn conditional_replace(
    source: &Path,
    target: &Path,
    expected: &[u8],
    replacement: &[u8],
    temporary_consumed: &mut bool,
) -> Result<ConditionalReplace> {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;

    let predecessor = unused_sibling_path(target)?;
    match replace_file_with_backup(target, source, &predecessor) {
        Ok(()) => *temporary_consumed = true,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND)
            ) =>
        {
            return Ok(ConditionalReplace { committed: false });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to replace {}", target.display()));
        }
    }

    let captured = fs::read(&predecessor).with_context(|| {
        format!(
            "failed to verify {}; its predecessor remains at {}",
            target.display(),
            predecessor.display()
        )
    })?;
    if captured == expected {
        fs::remove_file(&predecessor)?;
        return Ok(ConditionalReplace { committed: true });
    }

    let displaced = unused_sibling_path(target)?;
    if let Err(error) = replace_file_with_backup(target, &predecessor, &displaced) {
        anyhow::bail!(
            "{} changed during save and rollback failed: {error}; its predecessor remains at {}",
            target.display(),
            predecessor.display()
        );
    }
    let displaced_bytes = fs::read(&displaced).with_context(|| {
        format!(
            "failed to verify rollback of {}; displaced data remains at {}",
            target.display(),
            displaced.display()
        )
    })?;
    if displaced_bytes != replacement {
        anyhow::bail!(
            "{} changed more than once during save; the additional version remains at {}",
            target.display(),
            displaced.display()
        );
    }
    fs::remove_file(displaced)?;
    Ok(ConditionalReplace { committed: false })
}

#[cfg(windows)]
fn unused_sibling_path(target: &Path) -> Result<PathBuf> {
    let mut placeholder = TemporaryFile::create(target)?;
    drop(placeholder.file.take());
    fs::remove_file(placeholder.path())?;
    placeholder.replaced = true;
    Ok(placeholder.path.clone())
}

#[cfg(windows)]
fn replace_file_with_backup(
    target: &Path,
    replacement: &Path,
    backup: &Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
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
    }

    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let backup: Vec<u16> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: all paths are terminated UTF-16 buffers that remain live for the
    // call. ReplaceFileW atomically moves the displaced target to `backup`.
    let replaced = unsafe {
        replace_file_ffi(
            target.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{atomic_write, atomic_write_if_unchanged, prepare_write};
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

    #[test]
    fn conditionally_replaces_the_exact_predecessor() {
        let path = test_path();
        fs::write(&path, b"first").unwrap();

        atomic_write_if_unchanged(&path, Some(b"first"), b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rolls_back_a_conditional_replacement_of_a_newer_version() {
        let path = test_path();
        fs::write(&path, b"external").unwrap();

        let error = atomic_write_if_unchanged(&path, Some(b"stale"), b"editor").unwrap_err();

        assert!(format!("{error:#}").contains("changed on disk"));
        assert_eq!(fs::read(&path).unwrap(), b"external");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn catches_a_change_between_preparation_and_commit() {
        let path = test_path();
        fs::write(&path, b"expected").unwrap();
        let (target, mut temporary) = prepare_write(&path, b"editor").unwrap();

        fs::write(&path, b"external").unwrap();
        let committed = temporary
            .replace_if_unchanged(&target, b"expected", b"editor")
            .unwrap();

        assert!(!committed);
        assert_eq!(fs::read(&path).unwrap(), b"external");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn catches_creation_between_preparation_and_commit() {
        let path = test_path();
        let _ = fs::remove_file(&path);
        let (target, mut temporary) = prepare_write(&path, b"editor").unwrap();

        fs::write(&path, b"external").unwrap();
        let committed = temporary.install_if_missing(&target).unwrap();

        assert!(!committed);
        assert_eq!(fs::read(&path).unwrap(), b"external");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_only_if_the_destination_is_still_missing() {
        let path = test_path();
        let _ = fs::remove_file(&path);
        atomic_write_if_unchanged(&path, None, b"created").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"created");
        fs::remove_file(&path).unwrap();

        fs::write(&path, b"external").unwrap();
        let error = atomic_write_if_unchanged(&path, None, b"editor").unwrap_err();
        assert!(format!("{error:#}").contains("changed on disk"));
        assert_eq!(fs::read(&path).unwrap(), b"external");
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
