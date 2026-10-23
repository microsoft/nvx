//! The attribution of run policy paths belongs to OpenVMM, not to option order.

use proptest::prelude::*;

use super::*;

proptest! {
    #[test]
    fn policy_paths_attach_to_their_shares(
        events in prop::collection::vec((any::<bool>(), 0_u8..3, any::<bool>()), 0..40),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let roots = [directory.path().join("first"), directory.path().join("second")];
        for root in &roots {
            std::fs::create_dir(root).unwrap();
        }
        let mut arguments = Vec::new();
        let mut policies: [Vec<String>; 3] = Default::default();
        for (sequence, (second, kind, mount_first)) in events.into_iter().enumerate() {
            let root = &roots[usize::from(second)];
            let path = root.join(format!("path-{sequence}")).to_str().unwrap().to_owned();
            let option = ["--mount-deny", "--mount-allow", "--mount-write"][usize::from(kind)];
            let mount = format!("/share-{sequence},{},rw", root.display());
            let policy = [option.to_owned(), path.clone()];
            let share = ["--mount".to_owned(), mount];
            if mount_first {
                arguments.extend(share);
                arguments.extend(policy);
            } else {
                arguments.extend(policy);
                arguments.extend(share);
            }
            policies[usize::from(kind)].push(path);
        }
        // Two fixed shares exercise aggregate attribution even with an empty event sequence.
        // Restore avoids the unrelated cold-boot command-line budget.
        arguments.extend([
            "--mount".to_owned(), format!("/first,{},rw", roots[0].display()),
            "--mount".to_owned(), format!("/second,{},rw", roots[1].display()),
            "--restore-snapshot".to_owned(), "snapshot".to_owned(),
        ]);
        let (options, unrecognized) = parse(&arguments, "nvx-rs").unwrap();
        prop_assert!(unrecognized.is_empty());
        let rules = options.network.egress_rules().unwrap();
        let mut command = Vec::new();
        extend_shares_and_network(&mut command, &options, &rules).unwrap();
        for (kind, expected) in ["--mount-deny", "--mount-allow", "--mount-write"].into_iter().zip(policies) {
            let actual: Vec<_> = command.windows(2)
                .filter(|pair| pair[0] == kind).map(|pair| pair[1].clone()).collect();
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn aggregate_policy_paths_must_be_absolute(relative in "[a-z]{1,20}") {
        for option in ["--mount-deny", "--mount-allow", "--mount-write"] {
            let arguments = [
                "--mount", "/first,first",
                "--mount", "/second,second",
                option, &relative,
            ].map(str::to_owned);
            let (options, _) = parse(&arguments, "nvx-rs").unwrap();
            let rules = options.network.egress_rules().unwrap();
            prop_assert!(extend_shares_and_network(&mut Vec::new(), &options, &rules).is_err());
        }
    }
}
