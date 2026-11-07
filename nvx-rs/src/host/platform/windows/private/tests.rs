use std::os::windows::ffi::OsStrExt;

use super::super::wide;
use super::*;

fn long_directory() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mut parent = root.path().to_path_buf();
    while parent.as_os_str().encode_wide().count() < 320 {
        parent.push("long-state-root-component");
    }
    fs::create_dir_all(&parent).unwrap();
    (root, parent)
}

#[test]
fn private_directories_support_long_paths() {
    let (_root, parent) = long_directory();
    fs::create_dir(parent.join("child")).unwrap();
    for (index, parent) in [
        parent.clone(),
        parent.join("child").join(".."),
        fs::canonicalize(&parent).unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = parent.join(format!("store-{index}-\u{1f600}"));
        assert_eq!(create_store_dir(&directory).unwrap(), None);
        assert_eq!(create_store_dir(&directory).unwrap(), None);
        assert_eq!(check_store_dir(&directory).unwrap(), None);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("record"))
            .unwrap();
        assert_eq!(private_file_problem(&file).unwrap(), None);
    }
}

#[test]
fn renames_support_long_paths_without_replacing_existing_files() {
    let (_root, parent) = long_directory();
    let source = parent.join("source");
    let destination = parent.join("destination");
    fs::write(&source, b"original").unwrap();
    rename_no_replace(&source, &destination).unwrap();
    assert!(!source.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"original");

    fs::write(&source, b"replacement").unwrap();
    assert_eq!(
        rename_no_replace(&source, &destination).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&source).unwrap(), b"replacement");
    assert_eq!(fs::read(&destination).unwrap(), b"original");
}

#[test]
fn invalid_paths_do_not_create_or_rename_their_prefix() {
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("store\0suffix");
    assert_eq!(
        create_store_dir(&invalid).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(!root.path().join("store").exists());

    let source = root.path().join("source");
    fs::write(&source, b"original").unwrap();
    assert_eq!(
        rename_no_replace(&source, &invalid).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert!(!root.path().join("store").exists());
}

/// Replaces the access control list of the file or directory `path` with the protected list of
/// the descriptor `sddl`, in the security descriptor definition language.
pub(crate) fn set_access(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW};

    let sddl = wide(sddl);
    let name = wide_path(path)?;
    let mut descriptor = ptr::null_mut();
    // SAFETY: the string is NUL-terminated, and the output pointer is valid.
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
    let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    // SAFETY: the name is NUL-terminated, and the descriptor is valid.
    let set = unsafe { SetFileSecurityW(name.as_ptr(), information, descriptor) };
    let result = if set == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    // SAFETY: the conversion allocated the descriptor, which nothing uses any more.
    unsafe { LocalFree(descriptor) };
    result
}
