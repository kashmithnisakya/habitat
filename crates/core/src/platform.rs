//! What the operating systems do differently, for the things that the core's safety rests on. Other
//! trusted crates can use this too.
//!
//! Each function makes the same promise on every system. Where a system can't keep it fully, the
//! function says so, and so does `docs/threat-model.md`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use cap_std::fs::{Dir, OpenOptions as DirOpenOptions};

/// Creates a folder and any parents it needs. Only the current user can open a folder made by
/// [`create_private_dir`] or [`create_private_dirs`], and anything created inside it after that.
#[cfg(unix)]
pub(crate) fn create_private_dirs(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Creates a folder that only the current user can open, unless it's there already.
#[cfg(unix)]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
pub(crate) fn create_private_dirs(path: &Path) -> io::Result<()> {
    let existed = path.is_dir();
    fs::create_dir_all(path)?;
    if existed {
        return Ok(());
    }
    windows::make_private(path)
}

#[cfg(windows)]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    windows::make_private(path)
}

/// Sets how a file that only the core can write to is created.
pub(crate) fn private_file(options: &mut DirOpenOptions) -> &mut DirOpenOptions {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    options
}

/// Makes opening a file safe if it could be something else. On Unix, opening a named pipe would
/// otherwise wait forever. Windows doesn't have them in folders.
pub(crate) fn dont_block(options: &mut DirOpenOptions) -> &mut DirOpenOptions {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NONBLOCK);
    }
    options
}

/// Opens a file for reading, without following a link or waiting on a named pipe. On Windows, this
/// opens a link itself, so that the caller sees it isn't a regular file.
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

fn no_follow(options: &mut OpenOptions) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.custom_flags(windows::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(windows))]
    let _ = options;
}

/// Opens a file for reading and writing, creating it if needed, without following a link.
pub fn open_or_create_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Whether an error means that a link was refused, on systems that report it as its own error.
pub(crate) fn is_link_refused(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_STOPPED_ON_SYMLINK, ERROR_TOO_MANY_LINKS};

        // The first is what `cap-std` reports when it refuses a link itself.
        matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_TOO_MANY_LINKS | ERROR_STOPPED_ON_SYMLINK)
        )
    }
}

/// Creates a file that's new, with its contents saved to disk. A read-only file can only be read.
pub(crate) fn write_new_file(path: &Path, data: &[u8], read_only: bool) -> io::Result<()> {
    use std::io::Write;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(if read_only { 0o400 } else { 0o600 });
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    #[cfg(windows)]
    if read_only {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(true);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

/// Saves a folder's entries to disk, so that a file moved into it stays there after a power cut.
///
/// On Windows there's no reliable way to do this for an open folder, so this does nothing. NTFS
/// keeps its own journal, so a power cut can lose the latest move, but it can't leave a mix of the
/// old and new file. Being killed loses nothing, because the system has the move by then.
pub(crate) fn sync_dir(dir: &Dir) -> io::Result<()> {
    #[cfg(unix)]
    {
        // `Dir` holds a handle that can't be synced on Linux and Android, so open it again.
        dir.open(".")?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}

/// The same as [`sync_dir`], for a folder that isn't open.
pub fn sync_dir_path(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

/// Lets tests make links in the same way on every system.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io;
    use std::path::Path;

    pub(crate) fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            let (target, link) = (target.as_ref(), link.as_ref());
            let resolved = link.parent().unwrap_or(link).join(target);
            if resolved.is_dir() {
                std::os::windows::fs::symlink_dir(target, link)
            } else {
                std::os::windows::fs::symlink_file(target, link)
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    pub(super) use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    /// Gives the owner full access, and nobody else any, on this folder and whatever's created in
    /// it. "P" stops it inheriting anything else from its parent, "OW" is the owner, and "CO" is
    /// what makes whoever creates something inside the owner of that too.
    const OWNER_ONLY: &str = "D:P(A;OICI;FA;;;OW)(A;OICIIO;FA;;;CO)";

    /// Frees what Windows allocated for a security descriptor.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: this came from ConvertStringSecurityDescriptorToSecurityDescriptorW, whose
            // documentation says to free it with LocalFree, and it's only freed here.
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        text.as_ref().encode_wide().chain(iter::once(0)).collect()
    }

    /// Replaces a folder's access list. Windows only has a C API for this, so this needs `unsafe`.
    /// There's no way around it short of running `icacls`, which would be more fragile.
    pub(super) fn make_private(path: &Path) -> io::Result<()> {
        let sddl = wide(OWNER_ONLY);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is a valid string that ends in a zero, and `descriptor` is valid to write
        // to. The size is optional, and we pass null because we don't need it.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor = Descriptor(descriptor);

        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: the descriptor is valid for as long as `descriptor` lives, which is longer than
        // `dacl` is used for, and the other three are valid to write to.
        let found = unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        };
        if found == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            // A missing list would mean access for everyone, so this must never go ahead.
            return Err(io::ErrorKind::InvalidData.into());
        }

        let path = wide(path);
        // SAFETY: `path` is a valid string that ends in a zero, which this doesn't write to despite
        // its type. `dacl` is valid, as above, and the owner, group and audit list are optional.
        let result = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        if result != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn access_list(path: &Path) -> String {
        let output = Command::new("icacls").arg(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn a_private_folder_keeps_out_everyone_else() {
        let root = TempDir::new().unwrap();
        let folder = root.path().join("parent/private");
        create_private_dirs(&folder).unwrap();
        fs::write(folder.join("file"), "x").unwrap();
        fs::create_dir(folder.join("child")).unwrap();
        for path in [folder.clone(), folder.join("file"), folder.join("child")] {
            let list = access_list(&path);
            assert!(list.contains("OWNER RIGHTS"), "{list}");
            // These hold a backslash, or the path itself could look like one, such as C:\\Users. An
            // elevated process makes Administrators the owner of what it creates, which is what CI
            // does, and they could override any of this anyway, so that isn't checked for.
            for other in ["Everyone", "BUILTIN\\Users", "Authenticated Users"] {
                assert!(!list.contains(other), "{other} in {list}");
            }
        }
        // The owner can still use it.
        assert_eq!(fs::read(folder.join("file")).unwrap(), b"x");
    }

    #[test]
    fn a_private_folder_inside_a_private_folder_works() {
        let root = TempDir::new().unwrap();
        let folder = root.path().join("private");
        create_private_dirs(&folder).unwrap();
        create_private_dir(&folder.join("inner")).unwrap();
        fs::write(folder.join("inner/file"), "x").unwrap();
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_read_only_file_can_be_read_and_deleted_but_not_changed() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("file");
        write_new_file(&path, b"x", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x");
        assert!(fs::write(&path, "y").is_err());
        fs::remove_file(&path).unwrap();
    }
}
