use super::*;

#[test]
fn capabilities_are_named_without_their_prefix() {
    assert_eq!(capability_name(Capability::CHOWN), "chown");
    assert_eq!(
        capability_name(Capability::NET_BIND_SERVICE),
        "net-bind-service"
    );
    assert_eq!(
        capability_name(Capability::new(63).unwrap()),
        "capability 63"
    );
}

#[test]
fn status_fields_are_found_by_name() {
    let status = "Name:\tsh\nUid:\t0\t0\t0\t0\nCapEff:\t0000000000000001\nGroups:\t\n";
    assert_eq!(status_field(status, "Uid"), Some("0\t0\t0\t0"));
    assert_eq!(status_field(status, "CapEff"), Some("0000000000000001"));
    assert_eq!(status_field(status, "Groups"), Some(""));
    assert_eq!(status_field(status, "Cap"), None);
    assert_eq!(status_field(status, "Gid"), None);
}

#[test]
fn outcomes_name_the_error() {
    assert_eq!(outcome(Ok(())), "allowed");
    let refused = io::Error::from_raw_os_error(Errno::EROFS as i32);
    assert_eq!(outcome(Err(refused)), "refused EROFS");
    assert_eq!(outcome(Err(io::Error::other("odd"))), "failed odd");
}

#[test]
fn refusals_name_what_refused_the_identity() {
    assert_eq!(refusal(Ok(())), "accepted");
    let capability = Capability::new(21).unwrap();
    assert_eq!(
        refusal(Err(EntryError::Capability(capability))),
        "capability CAP_SYS_ADMIN"
    );
    assert_eq!(
        refusal(Err(EntryError::Published(Capability::NET_RAW))),
        "published CAP_NET_RAW"
    );
    assert_eq!(
        refusal(Err(EntryError::NonRootCapabilities)),
        "non-root-capabilities"
    );
}

#[test]
fn unknown_probes_and_arguments_are_refused() {
    let lowers = [PathBuf::from("/run/runtime-adapter/layers/distro")];
    let context = Context {
        runtime: Path::new("/run/runtime-adapter"),
        root: Path::new("/run/runtime-adapter/rootfs"),
        lowers: &lowers,
        upper: Path::new("/run/runtime-adapter/scratch"),
        first_partition: None,
        agent: Path::new("/sys/fs/cgroup/agent"),
        workload: Path::new("/sys/fs/cgroup/workload"),
        sibling: None,
    };
    assert_eq!(
        run(&Probe::parse("entry").unwrap(), &context),
        Err("the worker runs no probe named entry".to_owned())
    );
    assert_eq!(
        run(&Probe::parse("entries now").unwrap(), &context),
        Err("the entries probe takes no arguments".to_owned())
    );
}

#[test]
fn absolute_paths_lose_their_leading_slash() {
    assert_eq!(below_root(STAGED), Path::new(".runtime-adapter-guest"));
    assert_eq!(
        below_root("/tmp/runtime-adapter-entries/none"),
        Path::new("tmp/runtime-adapter-entries/none")
    );
}
