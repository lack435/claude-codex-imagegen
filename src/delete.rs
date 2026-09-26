//! Deleting through the handle that was checked (docs/design.md, "Safety rules").
//!
//! Checking a path and then deleting by path leaves a gap in which the path can come to name
//! something else. Cleanup instead opens what it means to delete once, and does everything through
//! that one handle:
//!
//! - It is opened with DELETE and read access, sharing with other readers only. While it is open,
//!   no one else can write, rename or delete the file; and the open itself fails, with a sharing
//!   violation, while another handle is writing it or does not share delete, which leaves the file
//!   for a later attempt.
//! - It is opened with `FILE_FLAG_OPEN_REPARSE_POINT`, so a link (a symbolic link or a junction)
//!   at the final name is opened as itself and never followed. Folders on the way there are still
//!   resolved, so [`Opened::final_path`] reports where the handle really is.
//! - Its type, size, content and location are read through the handle.
//! - The deletion is marked on the same handle (`SetFileInformationByHandle` with
//!   `FileDispositionInfo`) and takes effect as the handle closes, so the file verified is the file
//!   deleted.

use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::raw::c_void;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use crate::config::Fnv1a64;

const DELETE: u32 = 0x0001_0000;
const FILE_READ_DATA: u32 = 0x0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `FILE_INFO_BY_HANDLE_CLASS::FileDispositionInfo`.
const FILE_DISPOSITION_INFO_CLASS: i32 = 4;

extern "system" {
    fn SetFileInformationByHandle(
        file: *mut c_void,
        class: i32,
        info: *const c_void,
        size: u32,
    ) -> i32;
    fn GetFinalPathNameByHandleW(file: *mut c_void, path: *mut u16, len: u32, flags: u32) -> u32;
}

/// A file or folder opened for deletion, and what it was when opened.
pub struct Opened {
    file: File,
    meta: Metadata,
}

/// Open `path` for deletion, as the module comment describes. `Ok(None)` when nothing is there.
/// A file in use elsewhere without delete sharing is an error (a sharing violation), which
/// [`crate::output::retry_transient`] rides out for a moment.
pub fn open(path: &Path) -> io::Result<Option<Opened>> {
    let opened = OpenOptions::new()
        .access_mode(DELETE | FILE_READ_DATA | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        // BACKUP_SEMANTICS lets a folder be opened too, to be told apart from a file or removed.
        // It bypasses security checks only for a process that has enabled the backup and
        // restore privileges, which this one never does.
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    Ok(Some(Opened { file, meta }))
}

impl Opened {
    /// A reparse point: a symbolic link, a junction, or anything else that stands in for a file.
    fn is_link(&self) -> bool {
        self.meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn is_folder(&self) -> bool {
        self.meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// A file, and not a link of any kind.
    pub fn is_plain_file(&self) -> bool {
        !self.is_link() && !self.is_folder()
    }

    /// A folder, and not a link of any kind.
    pub fn is_plain_folder(&self) -> bool {
        !self.is_link() && self.is_folder()
    }

    /// The size when it was opened, which no one else can change while it is open.
    pub fn size(&self) -> u64 {
        self.meta.len()
    }

    /// The 64-bit FNV-1a of the content and the number of bytes read, both through this handle.
    pub fn fingerprint(&self) -> io::Result<(u64, u64)> {
        let mut hash = Fnv1a64::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        let mut file = &self.file;
        loop {
            match file.read(&mut buf) {
                Ok(0) => return Ok((hash.finish(), total)),
                Ok(n) => {
                    hash.update(&buf[..n]);
                    total += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Where the handle really is: its final path, with every link on the way resolved, in the
    /// `\\?\` form `std::fs::canonicalize` also returns.
    pub fn final_path(&self) -> io::Result<PathBuf> {
        let handle = self.file.as_raw_handle() as *mut c_void;
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: the handle is valid while `self.file` lives, and `buf` is writable for the
            // length passed. Flags 0: the normalized name with its drive letter.
            let n =
                unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0) }
                    as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                return Ok(PathBuf::from(OsString::from_wide(&buf[..n])));
            }
            // Too small: `n` is the size needed, terminating NUL included.
            buf.resize(n, 0);
        }
    }

    /// Delete it: mark the deletion on this handle, which takes effect as the handle closes here.
    /// A folder that is not empty fails with ERROR_DIR_NOT_EMPTY and is left as it was.
    pub fn delete(self) -> io::Result<()> {
        // FILE_DISPOSITION_INFO is one BOOLEAN, DeleteFile.
        let delete_file: u8 = 1;
        // SAFETY: the handle is valid while `self.file` lives, and was opened with DELETE access;
        // the buffer is the one-byte FILE_DISPOSITION_INFO the class expects and outlives the
        // call.
        let marked = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle() as *mut c_void,
                FILE_DISPOSITION_INFO_CLASS,
                (&delete_file as *const u8).cast(),
                1,
            )
        };
        if marked == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{make_junction, temp_dir};
    use std::fs;

    #[test]
    fn a_file_is_checked_and_deleted_through_one_handle() {
        let dir = temp_dir("delete");
        let path = dir.join("fox-v1.png");
        fs::write(&path, b"foobar").unwrap();
        let opened = open(&path).unwrap().expect("the file is there");
        assert!(opened.is_plain_file() && !opened.is_plain_folder());
        assert_eq!(opened.size(), 6);
        assert_eq!(opened.fingerprint().unwrap(), (0x8594_4171_f739_67e8, 6));
        let at = opened.final_path().unwrap();
        assert!(crate::cleanup::same_path(&at, &path), "{}", at.display());
        // While it is open, no one else can write or delete it.
        assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::remove_file(&path).is_err());
        opened.delete().unwrap();
        assert!(!path.exists());
        assert!(
            open(&path).unwrap().is_none(),
            "nothing there is not an error"
        );
    }

    #[test]
    fn a_file_held_without_delete_sharing_cannot_be_opened_for_deletion() {
        let dir = temp_dir("delete");
        let path = dir.join("held.png");
        fs::write(&path, b"x").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        let err = open(&path).err().expect("a sharing violation");
        assert_eq!(err.raw_os_error(), Some(32), "{err}");
        drop(held);
        open(&path).unwrap().unwrap().delete().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_junction_is_opened_as_itself_and_never_followed() {
        let dir = temp_dir("delete");
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.png"), b"keep").unwrap();
        let link = dir.join("link.png");
        make_junction(&link, &target);
        let opened = open(&link).unwrap().unwrap();
        assert!(!opened.is_plain_file() && !opened.is_plain_folder());
        drop(opened);
        // A plain folder is one; an empty one can be deleted through its handle, and one with
        // something in it cannot.
        let opened = open(&target).unwrap().unwrap();
        assert!(opened.is_plain_folder());
        let err = opened.delete().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(145), "ERROR_DIR_NOT_EMPTY: {err}");
        assert_eq!(fs::read(target.join("keep.png")).unwrap(), b"keep");
        let empty = dir.join("empty");
        fs::create_dir(&empty).unwrap();
        open(&empty).unwrap().unwrap().delete().unwrap();
        assert!(!empty.exists());
    }
}
