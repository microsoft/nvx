use super::*;

/// Every evidence, in the order of the module's table.
const ALL: [Evidence; 27] = [
    Evidence::AuthorizationDenied,
    Evidence::StaleBinding,
    Evidence::InvalidTargets,
    Evidence::InvalidFormat,
    Evidence::InvalidSettings,
    Evidence::InvalidActivate,
    Evidence::InvalidMappings,
    Evidence::UnservableImage,
    Evidence::UnusableScratch,
    Evidence::UnusableRoot,
    Evidence::SuppliedIdentity,
    Evidence::SuppliedLength,
    Evidence::SuppliedGeometry,
    Evidence::EndpointBind,
    Evidence::ForwardBind,
    Evidence::ResourceExhaustion,
    Evidence::IncompatibleHost,
    Evidence::RuntimeMismatch,
    Evidence::HostFailure,
    Evidence::LostStatus,
    Evidence::Timeout,
    Evidence::Reset,
    Evidence::ForcedKill,
    Evidence::Protocol,
    Evidence::MalformedGeneration,
    Evidence::TemplateDefect,
    Evidence::RepairStatus,
];

#[test]
fn each_row_of_the_table_attributes_its_evidence() {
    let rows: [(&[Evidence], Outcome); 6] = [
        (
            &[
                Evidence::AuthorizationDenied,
                Evidence::StaleBinding,
                Evidence::InvalidTargets,
                Evidence::InvalidFormat,
                Evidence::InvalidSettings,
                Evidence::InvalidActivate,
                Evidence::InvalidMappings,
                Evidence::UnservableImage,
                Evidence::UnusableScratch,
                Evidence::UnusableRoot,
            ],
            Outcome::Request,
        ),
        (
            &[
                Evidence::SuppliedIdentity,
                Evidence::SuppliedLength,
                Evidence::SuppliedGeometry,
            ],
            Outcome::Request,
        ),
        (
            &[
                Evidence::EndpointBind,
                Evidence::ForwardBind,
                Evidence::ResourceExhaustion,
                Evidence::IncompatibleHost,
                Evidence::RuntimeMismatch,
                Evidence::HostFailure,
            ],
            Outcome::Host,
        ),
        (
            &[
                Evidence::LostStatus,
                Evidence::Timeout,
                Evidence::Reset,
                Evidence::ForcedKill,
                Evidence::Protocol,
            ],
            Outcome::Unknown,
        ),
        (
            &[Evidence::MalformedGeneration, Evidence::TemplateDefect],
            Outcome::Template,
        ),
        (&[Evidence::RepairStatus], Outcome::Repair),
    ];
    let mut seen = Vec::new();
    for (evidence, outcome) in rows {
        for evidence in evidence {
            assert_eq!(evidence.outcome(), outcome, "{evidence}");
            assert_eq!(
                evidence.outcome().quarantines(),
                outcome == Outcome::Template,
                "{evidence}"
            );
            assert_eq!(
                evidence.is_supplied_file(),
                evidence.as_str().starts_with("supplied-"),
                "{evidence}"
            );
            seen.push(*evidence);
        }
    }
    // The table attributes every evidence once.
    assert_eq!(seen, ALL);
    // Only the template's outcome quarantines.
    for outcome in [
        Outcome::Request,
        Outcome::Staging,
        Outcome::Host,
        Outcome::Unknown,
        Outcome::Repair,
    ] {
        assert!(!outcome.quarantines(), "{outcome}");
    }
    assert!(Outcome::Template.quarantines());
}

#[test]
fn names_are_those_of_the_records() {
    for evidence in ALL {
        let json = serde_json::to_string(&evidence).unwrap();
        assert_eq!(json, format!("\"{evidence}\""));
        assert_eq!(serde_json::from_str::<Evidence>(&json).unwrap(), evidence);
    }
    let progress = [
        Progress::Planned,
        Progress::Launching,
        Progress::Spawned,
        Progress::Event,
        Progress::Acknowledged,
        Progress::Activating,
        Progress::Ready,
        Progress::Starting,
        Progress::Started,
        Progress::Committed,
    ];
    // The steps are in order, and spelled as their records spell them.
    assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
    for step in progress {
        assert_eq!(serde_json::to_string(&step).unwrap(), format!("\"{step}\""));
    }
    let stages: Vec<Stage> = progress.iter().map(|step| step.stage()).collect();
    assert_eq!(
        stages,
        [
            Stage::Plan,
            Stage::Launch,
            Stage::Restore,
            Stage::Activation,
            Stage::Activation,
            Stage::Activation,
            Stage::Start,
            Stage::Start,
            Stage::Commit,
            Stage::Run,
        ]
    );
    for stage in [
        Stage::Plan,
        Stage::Launch,
        Stage::Restore,
        Stage::Activation,
        Stage::Start,
        Stage::Commit,
        Stage::Run,
        Stage::Reattach,
    ] {
        assert_eq!(
            serde_json::to_string(&stage).unwrap(),
            format!("\"{stage}\"")
        );
    }
    for outcome in [
        Outcome::Request,
        Outcome::Staging,
        Outcome::Host,
        Outcome::Unknown,
        Outcome::Template,
        Outcome::Repair,
    ] {
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            format!("\"{outcome}\"")
        );
    }
}

#[test]
fn a_repair_status_alone_is_evidence_of_the_repair_stage_at_any_point() {
    for status in status::TIME_STATUSES.chain(status::REPAIR_STATUSES) {
        for progress in [Progress::Spawned, Progress::Ready, Progress::Committed] {
            assert_eq!(
                status_evidence(status, progress),
                Some(Evidence::RepairStatus),
                "{status} {progress}"
            );
        }
    }
}

#[test]
fn an_activation_status_is_evidence_of_its_class_until_a_workload_may_have_run() {
    let classes = [
        (status::ACTIVATION_DEADLINE, Evidence::Timeout),
        (status::ACTIVATION_PROTOCOL, Evidence::Protocol),
        (status::ACTIVATION_RECORD, Evidence::InvalidActivate),
        (status::ACTIVATION_RESOURCES, Evidence::InvalidTargets),
        (status::ACTIVATION_ROOT, Evidence::UnusableRoot),
        (status::ACTIVATION_WORKLOAD, Evidence::InvalidSettings),
        (status::ACTIVATION_EXPORT, Evidence::InvalidMappings),
        (status::ACTIVATION_NETWORK, Evidence::InvalidSettings),
        (status::ACTIVATION_ORDER, Evidence::Protocol),
    ];
    assert_eq!(
        classes.map(|(status, _)| status).to_vec(),
        status::ACTIVATION_STATUSES.collect::<Vec<_>>()
    );
    for (status, evidence) in classes {
        for progress in [Progress::Spawned, Progress::Activating, Progress::Ready] {
            assert_eq!(
                status_evidence(status, progress),
                Some(evidence),
                "{status} {progress}"
            );
        }
        // Once the start was sent, a workload may end the VM with any status.
        for progress in [Progress::Starting, Progress::Started, Progress::Committed] {
            assert_eq!(
                status_evidence(status, progress),
                None,
                "{status} {progress}"
            );
        }
    }
    // No other status is evidence: a success, the generic failure, or a workload's.
    for status in [0, 1, 2, 3, 39, 46, 49, 59, 137, 192, 196, 255] {
        assert_eq!(
            status_evidence(status, Progress::Activating),
            None,
            "{status}"
        );
    }
}

#[test]
fn a_report_gives_the_guest_s_status_and_its_evidence() {
    let report = |category: &str, status_code: i64| Report {
        category: Some(category.to_owned()),
        status_code: Some(status_code),
        teardown_complete: Some(true),
    };
    assert_eq!(reported(None, Progress::Event), (None, None));
    assert_eq!(
        reported(Some(&report("guest-exit", 195)), Progress::Event),
        (Some(Evidence::RepairStatus), Some(195))
    );
    assert_eq!(
        reported(Some(&report("guest-exit", 54)), Progress::Activating),
        (Some(Evidence::UnusableRoot), Some(54))
    );
    assert_eq!(
        reported(Some(&report("guest-exit", 54)), Progress::Committed),
        (None, Some(54))
    );
    assert_eq!(
        reported(Some(&report("success", 0)), Progress::Committed),
        (None, Some(0))
    );
    // A failure of the VMM, or of its teardown, has no status of the guest's.
    assert_eq!(
        reported(Some(&report("vmm-failure", 1)), Progress::Spawned),
        (None, None)
    );
    assert_eq!(
        reported(Some(&report("teardown-failure", 1)), Progress::Spawned),
        (None, None)
    );
    assert_eq!(
        reported(Some(&report("guest-exit", 256)), Progress::Spawned),
        (None, None)
    );
}

#[test]
fn a_status_prevails_over_what_was_seen_of_the_same_end_but_not_over_the_template_s_evidence() {
    let template = [Evidence::MalformedGeneration, Evidence::TemplateDefect];
    for evidence in ALL {
        // Without a status that is evidence, what the step saw stands.
        assert_eq!(prevailing(evidence, None), evidence, "{evidence}");
        for reported in [
            Evidence::RepairStatus,
            Evidence::UnusableRoot,
            Evidence::Timeout,
        ] {
            let prevailed = prevailing(evidence, Some(reported));
            // A check of the generation, or a diagnosis that attributes a defect to it, shows
            // the provenance that the status alone leaves to establish.
            let expected = if template.contains(&evidence) {
                evidence
            } else {
                reported
            };
            assert_eq!(prevailed, expected, "{evidence} {reported}");
        }
    }
}

#[test]
fn openvmm_s_fatal_error_is_evidence_of_an_incompatible_host_only_for_its_codes() {
    for code in INCOMPATIBLE_HOST_CODES {
        // OpenVMM leads its fatal error with the first time ABI code of the error's chain.
        let line = format!("fatal error: [{code}] failed to launch vm worker");
        assert_eq!(
            fatal_evidence(&line),
            Some(Evidence::IncompatibleHost),
            "{line}"
        );
    }
    // A code of the snapshot's time contract, or of this OpenVMM, does not tell the
    // generation's defect from the host's, and an error without a code tells nothing.
    for line in [
        "fatal error: [E_MANIFEST_TIME] failed to launch vm worker",
        "fatal error: [E_PROFILE_DIGEST] failed to launch vm worker",
        "fatal error: [E_SNAPSHOT_VERSION] failed to restore",
        "fatal error: failed to open the snapshot",
        "fatal error: [E_TSC_RATE_TOLERANCE failed to launch vm worker",
        // A line that is not the fatal error, such as a warning, names no code of it.
        "NVX-CPU-PROFILE-FALLBACK: [E_PROFILE_UNSUPPORTED] the host profile serves",
        "warning: fatal error: [E_CPU_GENERATION] quoted",
    ] {
        assert_eq!(fatal_evidence(line), None, "{line}");
    }
}

#[test]
fn openvmm_s_fatal_error_is_evidence_of_a_forward_bind_when_it_names_a_forward_it_cannot_bind() {
    // The cause of the chain follows the line that leads the error, or is the line's own.
    for line in [
        "fatal error: failed to launch vm worker: failed to create socket for port forward \
             (Tcp 127.0.0.1:18080)",
        "fatal error: failed to create socket for port forward (Udp 127.0.0.1:5353)",
        "fatal error: [E_MANIFEST_TIME] failed to launch vm worker: failed to create socket \
             for port forward (Tcp 127.0.0.1:18080)",
    ] {
        assert_eq!(fatal_evidence(line), Some(Evidence::ForwardBind), "{line}");
    }
    // A code of an incompatible host prevails, and a line that is not the fatal error tells
    // nothing.
    let incompatible = "fatal error: [E_CPU_GENERATION] failed to launch vm worker: failed to \
                            create socket for port forward (Tcp 127.0.0.1:18080)";
    assert_eq!(
        fatal_evidence(incompatible),
        Some(Evidence::IncompatibleHost)
    );
    let quoted = "warning: failed to create socket for port forward (Tcp 127.0.0.1:18080)";
    assert_eq!(fatal_evidence(quoted), None);
}

#[test]
fn openvmm_s_fatal_error_is_evidence_of_a_forward_bind_when_it_leads_with_its_code() {
    // OpenVMM leads its fatal error with the code of a forward that it cannot bind, which leads
    // the cause of the error's chain that names the forward too, which may follow the line.
    for line in [
        "fatal error: [E_PORT_FORWARD_BIND] failed to launch vm worker",
        "fatal error: [E_PORT_FORWARD_BIND] failed to launch vm worker: failed to create socket \
         for port forward (Tcp 127.0.0.1:18080)",
        "fatal error: [E_PORT_FORWARD_BIND] failed to create socket for port forward \
         (Udp 127.0.0.1:5353)",
    ] {
        assert_eq!(fatal_evidence(line), Some(Evidence::ForwardBind), "{line}");
    }
    // Only the code that leads the fatal error tells it: another code, the code after the line's
    // start, or a line that is not the fatal error tells nothing.
    for line in [
        "fatal error: [E_PORT_FORWARD_BINDING] failed to launch vm worker",
        "fatal error: failed to launch vm worker: [E_PORT_FORWARD_BIND] quoted",
        "warning: [E_PORT_FORWARD_BIND] failed to launch vm worker",
    ] {
        assert_eq!(fatal_evidence(line), None, "{line}");
    }
}

#[test]
fn host_failures_that_ran_out_of_a_resource_are_resource_exhaustion() {
    let failed = |kind: io::ErrorKind| {
        host_evidence(&Error::io("cannot write").with_source(io::Error::from(kind)))
    };
    assert_eq!(
        failed(io::ErrorKind::StorageFull),
        Evidence::ResourceExhaustion
    );
    assert_eq!(
        failed(io::ErrorKind::OutOfMemory),
        Evidence::ResourceExhaustion
    );
    assert_eq!(
        failed(io::ErrorKind::PermissionDenied),
        Evidence::HostFailure
    );
    assert_eq!(
        host_evidence(&Error::io("no source")),
        Evidence::HostFailure
    );
    // A cause deeper in the chain counts too.
    let nested = Error::io("outer")
        .with_source(Error::io("inner").with_source(io::Error::from(io::ErrorKind::StorageFull)));
    assert_eq!(host_evidence(&nested), Evidence::ResourceExhaustion);
}

#[test]
fn teardowns_follow_the_end_of_the_launch() {
    assert_eq!(Teardown::of(Ok(Recovery::Exited)), Teardown::Exited);
    assert_eq!(Teardown::of(Ok(Recovery::Clean)), Teardown::Exited);
    assert_eq!(
        Teardown::of(Ok(Recovery::Indeterminate("held".to_owned()))),
        Teardown::Unresolved("held".to_owned())
    );
    let failed = Teardown::of(Err(Error::io("cannot read")));
    assert!(!failed.is_resolved());
    assert!(!Teardown::Pending.is_resolved());
    assert!(Teardown::NoVm.is_resolved());
    assert!(Teardown::Terminated.is_resolved());
}
