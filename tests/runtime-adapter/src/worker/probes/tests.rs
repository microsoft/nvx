use super::*;

#[test]
fn a_mount_table_is_read_with_its_escapes() {
    let table = "\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
41 30 254:0 / /run/a\\040b/layers/distro ro,relatime - erofs /dev/vda ro,user_xattr
42 30 254:16 / /run/a\\040b/scratch rw,nosuid,nodev,relatime - ext4 /dev/vdb rw
43 30 0:40 / /run/a\\040b/rootfs rw,nosuid,nodev,relatime - overlay overlay \
rw,lowerdir=/run/a\\040b/layers/distro,metacopy=on,xino=on
";
    let table = mount_table(table).unwrap();
    assert_eq!(table.len(), 4);
    assert_eq!(
        table[1],
        Mount {
            point: PathBuf::from("/run/a b/layers/distro"),
            options: "ro,relatime".to_owned(),
            kind: "erofs".to_owned(),
            source: "/dev/vda".to_owned(),
            super_options: "ro,user_xattr".to_owned(),
        }
    );
    assert_eq!(table[0].point, Path::new("/proc"));
    assert_eq!(table[3].kind, "overlay");
    assert_eq!(mount_table("").unwrap(), []);
    for line in [
        "1 2 3 4 5 6 - a b",
        "1 2 3 4 - a b c",
        "1 2 3 4 5 6 7 a b c",
    ] {
        let error = mount_table(line).unwrap_err();
        assert!(error.to_string().contains("malformed line"), "{line:?}");
    }
}

#[test]
fn escapes_are_three_octal_digits() {
    assert_eq!(unescape("a\\040b\\134c"), b"a b\\c");
    assert_eq!(unescape("\\012\\011"), b"\n\t");
    // What starts no escape stays as it is.
    assert_eq!(unescape("a\\08b\\04"), b"a\\08b\\04");
    assert_eq!(unescape("\\"), b"\\");
}

#[test]
fn process_records_are_read_as_the_kernel_writes_them() {
    assert_eq!(parse_stat("17 (a) b (c)) S 1 17 17 0 -1"), Some(("S", "1")));
    assert_eq!(parse_stat("17 (idle) R 1"), Some(("R", "1")));
    assert_eq!(parse_stat("17 (idle) R"), None);
    assert_eq!(parse_stat("17 idle R 1"), None);
    assert_eq!(unified_cgroup("0::/agent\n"), Some("/agent"));
    assert_eq!(unified_cgroup("1:name=x:/a\n0::/\n"), Some("/"));
    assert_eq!(unified_cgroup("1:name=x:/a\n"), None);
    assert_eq!(
        command_text(b"/sbin/runtime-adapter-guest\0idle\0"),
        "/sbin/runtime-adapter-guest idle"
    );
    assert_eq!(command_text(b""), "");
}

#[test]
fn the_memory_total_is_read_in_bytes() {
    let text = "MemTotal:         241540 kB\nMemFree:  1 kB\n";
    assert_eq!(memory_total(text), Some(241_540 * 1024));
    assert_eq!(memory_total("MemFree: 1 kB\n"), None);
    assert_eq!(memory_total("MemTotal: 1 MB\n"), None);
    assert_eq!(memory_total("MemTotal: x kB\n"), None);
}

#[test]
fn the_os_release_id_loses_its_quotes() {
    let text = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
    assert_eq!(os_release_id(text), Some("ubuntu"));
    assert_eq!(os_release_id("ID=\"alpine\"\n"), Some("alpine"));
    assert_eq!(os_release_id("ID='x'"), Some("x"));
    assert_eq!(os_release_id("NAME=x\n"), None);
}

#[test]
fn the_kmsg_probe_takes_one_argument() {
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
    let mut held = Vec::new();
    for text in ["kmsg", "kmsg first second"] {
        let probe = Probe::parse(text).unwrap();
        assert_eq!(
            run(&probe, &context, &mut held),
            Err("the kmsg probe takes one argument".to_owned()),
            "{text}"
        );
    }
    assert!(held.is_empty());
}

#[test]
fn the_kmsg_probe_keeps_its_text_out_of_the_record_s_priority() {
    // The kernel's log takes a record that starts with `<N>` for one of priority N, and drops the
    // prefix, so a text that starts so would not reach the console as it is.
    for text in ["first-996", "<0>first-996", "<12>"] {
        let record = kmsg_record(text);
        assert_eq!(record, format!("{KMSG_PREFIX}{text}\n"));
        assert!(!record.starts_with('<'), "{record:?}");
    }
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
    let mut held = Vec::new();
    let unknown = Probe::parse("mapping").unwrap();
    assert_eq!(
        run(&unknown, &context, &mut held),
        Err("the worker runs no probe named mapping".to_owned())
    );
    let with_argument = Probe::parse("hold /").unwrap();
    assert_eq!(
        run(&with_argument, &context, &mut held),
        Err("the hold probe takes no arguments".to_owned())
    );
    let with_argument = Probe::parse("first-partition 2").unwrap();
    assert_eq!(
        run(&with_argument, &context, &mut held),
        Err("the first-partition probe takes no arguments".to_owned())
    );
    for name in ["reads", "read-disks", "memory", "processors", "time"] {
        let with_argument = Probe::parse(&format!("{name} vda")).unwrap();
        assert_eq!(
            run(&with_argument, &context, &mut held),
            Err(format!("the {name} probe takes no arguments"))
        );
    }
    for (text, problem) in [
        ("copy-up", "the copy-up probe takes one argument"),
        ("rename a b", "the rename probe takes one argument"),
        (
            "copy-up /etc/os-release",
            "the copy-up probe takes a relative path in the root, not \"/etc/os-release\"",
        ),
        (
            "rename ../mnt",
            "the rename probe takes a relative path in the root, not \"../mnt\"",
        ),
        (
            "rename ./mnt",
            "the rename probe takes a relative path in the root, not \"./mnt\"",
        ),
        (
            "copy-up usr/../etc",
            "the copy-up probe takes a relative path in the root, not \"usr/../etc\"",
        ),
    ] {
        let probe = Probe::parse(text).unwrap();
        assert_eq!(run(&probe, &context, &mut held), Err(problem.to_owned()));
    }
    assert_eq!(
        path_argument("rename", "lost+found"),
        Ok(Path::new("lost+found"))
    );
    assert_eq!(
        path_argument("copy-up", "usr/lib/os-release"),
        Ok(Path::new("usr/lib/os-release"))
    );
    assert!(held.is_empty());
}

#[test]
fn the_sectors_read_are_the_third_field_of_a_disk_s_statistics() {
    let stat = "     163        0     8530      120        0        0        0        0 \
                    0      168      120        0        0        0        0        0        0";
    assert_eq!(sectors_read(stat), Some(8530));
    assert_eq!(sectors_read("1 2"), None);
    assert_eq!(sectors_read("1 2 x"), None);
    assert_eq!(sectors_read("1 2 -3"), None);
    assert_eq!(sectors_read("absent"), None);
}

#[test]
fn the_time_state_s_lines_are_facts_with_their_keys_in_the_facts_form() {
    let state = "version=1\nrestore_status=ok\nlast_downtime_source=monotonic\n\
                     Bad_Key=x\nempty=\nrestore_status=fail\nno value\nlast_step_ns=-2\n";
    let facts = time_facts(state);
    let expected = [
        ("version", "1"),
        ("restore-status", "ok"),
        ("last-downtime-source", "monotonic"),
        ("empty", ""),
        ("last-step-ns", "-2"),
    ];
    assert_eq!(facts.iter().collect::<Vec<_>>(), expected);
    assert!(time_facts("").is_empty());
}

#[test]
fn the_first_partition_is_reported_in_hexadecimal() {
    let lowers = [
        PathBuf::from("/run/runtime-adapter/layers/p2"),
        PathBuf::from("/run/runtime-adapter/layers/p3"),
    ];
    let bytes = b"\xff\x00a \x80";
    let mut context = Context {
        runtime: Path::new("/run/runtime-adapter"),
        root: Path::new("/run/runtime-adapter/rootfs"),
        lowers: &lowers,
        upper: Path::new("/run/runtime-adapter/tmpfs"),
        first_partition: Some(bytes),
        agent: Path::new("/sys/fs/cgroup/agent"),
        workload: Path::new("/sys/fs/cgroup/workload"),
        sibling: None,
    };
    let probe = Probe::parse("first-partition").unwrap();
    let mut held = Vec::new();
    let mut expected = Facts::new();
    expected.push("length", "5");
    expected.push("bytes", "ff00612080");
    assert_eq!(run(&probe, &context, &mut held), Ok(expected));
    context.first_partition = Some(b"");
    let mut expected = Facts::new();
    expected.push("length", "0");
    expected.push("bytes", "");
    assert_eq!(run(&probe, &context, &mut held), Ok(expected));
    context.first_partition = None;
    let mut expected = Facts::new();
    expected.push("length", "none");
    expected.push("bytes", "none");
    assert_eq!(run(&probe, &context, &mut held), Ok(expected));
    assert_eq!(hexadecimal(&[0x0f, 0xa0, 0x5c]), "0fa05c");
}

#[test]
fn the_cpus_that_a_restore_brought_online_are_those_of_its_record() {
    assert_eq!(restored_cpus("generation=3\nnew_cpus=1-3\n"), "1-3");
    assert_eq!(restored_cpus("generation=3\nnew_cpus=none\n"), "none");
    assert_eq!(
        restored_cpus("generation=3\n"),
        "unreadable: the record names no new CPUs"
    );
}
