use std::io::{Read, Write};

use super::*;

#[test]
fn metadata_opens_need_no_content_access_or_data_sharing() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file");
    fs::write(&path, b"contents")?;
    let _exclusive = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)?;

    let mut file = open_for_metadata(&path)?;
    assert_eq!(file.metadata()?.len(), 8);
    assert!(matches!(
        file.read(&mut [0]),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied
    ));
    assert!(matches!(
        file.write(b"x"),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied
    ));
    Ok(())
}

#[test]
fn metadata_opens_accept_directories_and_follow_their_links() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    fs::create_dir(&target)?;
    crate::host::tests::junction(&link, &target);

    let target = open_for_metadata(&target)?;
    let link = open_for_metadata(&link)?;
    assert!(target.metadata()?.is_dir());
    assert!(link.metadata()?.is_dir());
    assert_eq!(opened_file_id(&target)?, opened_file_id(&link)?);
    Ok(())
}

#[test]
fn metadata_opens_preserve_open_failures() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    for (name, kind) in [
        ("missing", io::ErrorKind::NotFound),
        ("invalid\0name", io::ErrorKind::InvalidInput),
    ] {
        let path = directory.path().join(name);
        let expected = fs::File::open(&path);
        let actual = open_for_metadata(&path);
        match (actual, expected) {
            (Err(actual), Err(expected)) => {
                assert_eq!(actual.kind(), kind);
                assert_eq!(actual.raw_os_error(), expected.raw_os_error());
                assert_eq!(actual.to_string(), expected.to_string());
            }
            _ => return Err(io::Error::other("an invalid path unexpectedly opened")),
        }
    }
    Ok(())
}
