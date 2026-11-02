use super::*;

fn entries(environment: &[CString]) -> Vec<&[u8]> {
    environment.iter().map(|entry| entry.as_bytes()).collect()
}

#[test]
fn setting_replaces_the_first_entry_in_place_or_appends_one() {
    let mut environment = vec![
        c"A=1".to_owned(),
        c"AB=2".to_owned(),
        c"B=3".to_owned(),
        c"A=4".to_owned(),
    ];
    set(&mut environment, b"A", b"5").unwrap();
    set(&mut environment, b"C", b"").unwrap();
    assert_eq!(
        entries(&environment),
        [b"A=5".as_slice(), b"AB=2", b"B=3", b"A=4", b"C="]
    );
    assert_eq!(set(&mut environment, b"D", b"\0"), Err(Errno::EINVAL));
}

#[test]
fn the_inherited_environment_has_every_variable() {
    let environment = inherited().unwrap();
    assert_eq!(environment.len(), std::env::vars_os().count());
    assert!(
        environment
            .iter()
            .all(|entry| entry.as_bytes().contains(&b'='))
    );
}
