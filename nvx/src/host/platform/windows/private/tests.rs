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
        #[cfg(feature = "cli")]
        assert_eq!(check_store_dir(&directory).unwrap(), None);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("record"))
            .unwrap();
        assert_eq!(private_file_problem(&file).unwrap(), None);
        let vm = directory.join("vm");
        assert_eq!(create_vm_dir(&vm).unwrap(), None);
        assert_eq!(check_vm_dir(&vm).unwrap(), None);
    }
}

#[cfg(feature = "cli")]
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

#[cfg(feature = "cli")]
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

fn take_wide(text: *mut u16) -> String {
    use windows_sys::Win32::Foundation::LocalFree;

    // SAFETY: the conversion returns a readable NUL-terminated string.
    let len = (0..)
        .take_while(|&index| unsafe { *text.add(index) } != 0)
        .count();
    // SAFETY: the string holds `len` units before its NUL.
    let string = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).unwrap();
    // SAFETY: the conversion allocated this string, which is no longer used.
    unsafe { LocalFree(text.cast()) };
    string
}

pub(crate) fn user_sid() -> String {
    let user = CurrentUser::get().unwrap();
    sddl_sid(user.sid())
}

fn sddl_sid(sid: PSID) -> String {
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };

    // Use the same SID aliases as `access`'s descriptor snapshots.
    // SAFETY: SECURITY_DESCRIPTOR is plain data with a valid all-zero representation.
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { mem::zeroed() };
    let descriptor: PSECURITY_DESCRIPTOR = (&raw mut descriptor).cast();
    // SAFETY: the descriptor and SID are valid, and the SID outlives the conversion.
    let initialized = unsafe {
        InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorOwner(descriptor, sid, 0) != 0
    };
    assert!(initialized, "{}", io::Error::last_os_error());
    let mut text = ptr::null_mut();
    // SAFETY: the descriptor and output pointer are valid; only its owner is requested.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION,
            &mut text,
            ptr::null_mut(),
        )
    };
    assert_ne!(converted, 0, "{}", io::Error::last_os_error());
    take_wide(text)
        .strip_prefix("O:")
        .expect("the descriptor reports its owner")
        .to_owned()
}

#[test]
fn security_identifier_spelling_matches_the_descriptor_snapshot() {
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, SECURITY_MAX_SID_SIZE, WinLocalServiceSid,
    };

    for (kind, expected) in [(WinLocalSystemSid, "SY"), (WinLocalServiceSid, "LS")] {
        let mut buffer =
            vec![0u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(mem::size_of::<u64>())];
        let sid: PSID = buffer.as_mut_ptr().cast();
        let mut length = (buffer.len() * mem::size_of::<u64>()) as u32;
        // SAFETY: the aligned buffer holds at least SECURITY_MAX_SID_SIZE bytes.
        let created = unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid, &mut length) };
        assert_ne!(created, 0, "{}", io::Error::last_os_error());
        assert_eq!(sddl_sid(sid), expected);
    }
}

pub(crate) fn access(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::GetFileSecurityW;

    let name = wide_path(path)?;
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut len = 0;
    // SAFETY: the path is terminated; an empty buffer requests the descriptor's length.
    unsafe { GetFileSecurityW(name.as_ptr(), information, ptr::null_mut(), 0, &mut len) };
    let mut buffer = vec![0u64; (len as usize).div_ceil(mem::size_of::<u64>())];
    let descriptor: PSECURITY_DESCRIPTOR = buffer.as_mut_ptr().cast();
    // SAFETY: the path is terminated and the aligned buffer holds `len` bytes.
    if unsafe { GetFileSecurityW(name.as_ptr(), information, descriptor, len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut text = ptr::null_mut();
    // SAFETY: the descriptor and output pointers are valid.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            information,
            &mut text,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take_wide(text))
}

#[test]
fn vm_directories_must_be_owned_protected_and_allow_only_the_current_user_full_inherited_access() {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    let user = CurrentUser::get().unwrap();
    let problem = |sddl: &str| {
        let text = wide(sddl);
        let mut descriptor = ptr::null_mut();
        // SAFETY: the text is terminated and the output pointer is valid.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{sddl}: {}", io::Error::last_os_error());
        let problem = vm_dir_problem(descriptor, &user).unwrap();
        // SAFETY: the conversion allocated this descriptor, which is no longer used.
        unsafe { LocalFree(descriptor) };
        problem
    };
    let sid = user_sid();
    let full = format!("(A;OICI;FA;;;{sid})");
    for private in [
        format!("O:{sid}D:P{full}"),
        format!("O:{sid}D:PAI{full}"),
        format!("O:{sid}D:P(A;OICI;GA;;;{sid})"),
        format!("O:{sid}D:P(A;OICIIO;GR;;;{sid}){full}(A;;FR;;;{sid})"),
    ] {
        assert_eq!(problem(&private), None, "{private}");
    }
    let others = "allows others than the current user";
    let partial = "does not allow the current user full access that the files and directories created in it \
         inherit";
    for (shared, expected) in [
        (format!("O:SYD:P{full}"), "belongs to another user"),
        (format!("O:BAD:P{full}"), "belongs to another user"),
        (format!("O:{sid}"), "allows every user"),
        (format!("O:{sid}D:NO_ACCESS_CONTROL"), "allows every user"),
        (
            format!("O:{sid}D:{full}"),
            "inherits access control entries from its parent",
        ),
        (
            format!("O:{sid}D:AI{full}"),
            "inherits access control entries from its parent",
        ),
        (format!("O:{sid}D:P{full}(A;OICI;FA;;;SY)"), others),
        (format!("O:{sid}D:P{full}(A;OICI;FA;;;BA)"), others),
        (format!("O:{sid}D:P{full}(A;OICIIO;FA;;;CO)"), others),
        (format!("O:{sid}D:P{full}(A;;RC;;;OW)"), others),
        (format!("O:{sid}D:P{full}(A;;FR;;;WD)"), others),
        (
            format!("O:{sid}D:P(D;;WD;;;{sid}){full}"),
            "has an access control entry of type 1, not one that allows access",
        ),
        (format!("O:{sid}D:P"), partial),
        // GENERIC_READ's numeric value exceeds FILE_ALL_ACCESS but grants none of its rights.
        (format!("O:{sid}D:P(A;OICI;GR;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;OICI;0x1f01fe;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;;FA;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;OI;FA;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;CI;FA;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;OICIIO;FA;;;{sid})"), partial),
        (format!("O:{sid}D:P(A;OICINP;FA;;;{sid})"), partial),
    ] {
        assert_eq!(problem(&shared).as_deref(), Some(expected), "{shared}");
    }
}
