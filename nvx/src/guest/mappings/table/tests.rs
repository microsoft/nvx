use std::cell::Cell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::*;
use crate::protocol::{MAP_READ_ONLY, MAPS_HEADER_LEN};

/// Returns a `MAPS` payload whose first entry is the table's `first`, of `entries`, each a
/// source, a target, and its flags, which says that it holds `count` entries, if given, and
/// otherwise as many as it does.
fn part(first: u32, entries: &[(&[u8], &[u8], u16)], count: Option<u32>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&first.to_le_bytes());
    payload.extend_from_slice(&count.unwrap_or(entries.len() as u32).to_le_bytes());
    for (source, target, flags) in entries {
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
        payload.extend_from_slice(&(target.len() as u16).to_le_bytes());
        payload.extend_from_slice(source);
        payload.extend_from_slice(target);
    }
    payload
}

/// Returns the part of the table's entries from `first` up to `end`, each the read-write
/// mapping of the export's `N` at `/srv/N`.
fn numbered(first: u32, end: u32) -> Vec<u8> {
    let paths: Vec<(String, String)> = (first..end)
        .map(|entry| (entry.to_string(), format!("/srv/{entry}")))
        .collect();
    let entries: Vec<(&[u8], &[u8], u16)> = paths
        .iter()
        .map(|(source, target)| (source.as_bytes(), target.as_bytes(), 0))
        .collect();
    part(first, &entries, None)
}

/// Returns the targets of `mappings`, relative to the root, as text.
fn targets(mappings: &[Mapping]) -> Vec<String> {
    mappings
        .iter()
        .map(|mapping| mapping.target().display().to_string())
        .collect()
}

/// Maps nothing and succeeds, as a root that applies every mapping does.
fn applies(_: &[Mapping]) -> Result<(), (usize, &'static str)> {
    Ok(())
}

#[test]
fn a_table_takes_the_count_that_the_host_announced() {
    let table = MappingTable::announced(b"quiet nvx_maps=3 console=hvc0\n").unwrap();
    assert_eq!((table.expected(), table.applied()), (3, 0));
    assert!(!table.is_latched() && !table.is_complete());
    // A command line without the token announces none, and such a table is complete.
    let table = MappingTable::announced(b"quiet nvx_map=0/src:/src").unwrap();
    assert_eq!(table, MappingTable::default());
    assert!(table.is_complete());
    for refused in [
        &b"nvx_maps=0"[..],
        b"nvx_maps=4097",
        b"nvx_maps=x",
        b"nvx_maps=",
        b"nvx_maps=1 nvx_maps=1",
    ] {
        assert!(
            MappingTable::announced(refused).is_err(),
            "{}",
            String::from_utf8_lossy(refused)
        );
    }
    // An adapter announces up to as many as the protocol allows.
    let largest = MAX_HOST_MAPPINGS as u32;
    assert_eq!(MappingTable::new(largest).unwrap().expected(), largest);
    assert_eq!(MappingTable::new(largest + 1), None);
    assert!(MappingTable::new(0).unwrap().is_complete());
}

#[test]
fn parts_apply_in_order_until_the_table_is_complete() {
    let mut table = MappingTable::new(5).unwrap();
    let mut handed = Vec::new();
    for (first, end) in [(0, 2), (2, 3), (3, 5)] {
        table
            .apply(&numbered(first, end), |mappings| {
                handed.push(targets(mappings));
                applies(mappings)
            })
            .unwrap();
        assert_eq!(table.applied(), end);
        assert_eq!(table.is_complete(), end == 5);
    }
    let expected: [&[&str]; 3] = [&["srv/0", "srv/1"], &["srv/2"], &["srv/3", "srv/4"]];
    assert_eq!(handed, expected);
    // A complete table takes no further part.
    let refused = table.apply(&numbered(5, 6), applies).unwrap_err();
    assert_eq!(
        refused,
        TableError::Refused {
            entry: 5,
            refusal: Refusal::Excess {
                end: 6,
                announced: 5,
            },
        }
    );
    assert!(table.is_complete());
}

#[test]
fn mappings_keep_the_paths_and_the_access_of_their_entries() {
    let mut table = MappingTable::new(2).unwrap();
    let payload = part(
        0,
        &[
            (b"0/src", b"/workspace", MAP_READ_ONLY),
            (b"1/\xff", b"/data/\xfe", 0),
        ],
        None,
    );
    table
        .apply(&payload, |mappings| {
            let parts: Vec<_> = mappings
                .iter()
                .map(|mapping| {
                    (
                        mapping.source().as_os_str().as_bytes(),
                        mapping.target().as_os_str().as_bytes(),
                        mapping.access(),
                        mapping.kind(),
                    )
                })
                .collect();
            assert_eq!(
                parts,
                [
                    (&b"0/src"[..], &b"workspace"[..], Access::ReadOnly, None),
                    (b"1/\xff", b"data/\xfe", Access::ReadWrite, None),
                ]
            );
            applies(mappings)
        })
        .unwrap();
    assert!(table.is_complete());
}

#[test]
fn refused_parts_map_nothing_and_change_nothing() {
    let good = (&b"0/src"[..], &b"/srv/src"[..], 0);
    let cases: [(&str, u32, Vec<u8>, Refusal); 8] = [
        (
            "a part that does not decode",
            3,
            vec![0; 3],
            Refusal::Malformed(protocol::decode_guest_maps_payload(&[0; 3]).unwrap_err()),
        ),
        (
            "a part with trailing bytes",
            3,
            [part(0, &[good], None), vec![0]].concat(),
            Refusal::Malformed(
                protocol::decode_guest_maps_payload(&[part(0, &[good], None), vec![0]].concat())
                    .unwrap_err(),
            ),
        ),
        (
            "a part without an entry",
            3,
            vec![0; MAPS_HEADER_LEN],
            Refusal::Empty,
        ),
        (
            "a part that skips entries",
            3,
            numbered(1, 2),
            Refusal::Order {
                first: 1,
                expected: 0,
            },
        ),
        (
            "a part past the announcement",
            3,
            numbered(0, 4),
            Refusal::Excess {
                end: 4,
                announced: 3,
            },
        ),
        (
            "a part when the host announced none",
            0,
            numbered(0, 1),
            Refusal::Excess {
                end: 1,
                announced: 0,
            },
        ),
        (
            "a part that maps the root of the export",
            3,
            part(0, &[good, (b"", b"/srv/root", 0)], None),
            Refusal::ExportRoot { entry: 1 },
        ),
        (
            "a part that maps the root of the export first",
            3,
            part(0, &[(b"", b"/srv/root", MAP_READ_ONLY)], None),
            Refusal::ExportRoot { entry: 0 },
        ),
    ];
    for (name, expected, payload, refusal) in cases {
        let mut table = MappingTable::new(expected).unwrap();
        let before = table.clone();
        let called = Cell::new(false);
        let refused = table.apply(&payload, |mappings| {
            called.set(true);
            applies(mappings)
        });
        assert_eq!(
            refused,
            Err(TableError::Refused { entry: 0, refusal }),
            "{name}"
        );
        assert!(!called.get(), "{name}");
        assert_eq!(table, before, "{name}");
    }
}

#[test]
fn a_refused_part_leaves_the_table_to_go_on() {
    // A part that repeats entries the table applied is refused at the entry that it expects
    // next, after which the right part applies.
    let mut table = MappingTable::new(4).unwrap();
    table.apply(&numbered(0, 2), applies).unwrap();
    let refused = table.apply(&numbered(0, 2), applies).unwrap_err();
    assert_eq!(
        refused,
        TableError::Refused {
            entry: 2,
            refusal: Refusal::Order {
                first: 0,
                expected: 2,
            },
        }
    );
    assert_eq!(refused.entry(), 2);
    assert!(!table.is_latched());
    table.apply(&numbered(2, 4), applies).unwrap();
    assert!(table.is_complete());
}

#[test]
fn a_failed_mapping_keeps_those_before_it_and_latches_the_table() {
    // The root applies the part's first mapping, then fails at its second, the table's
    // fourth.
    let mut table = MappingTable::new(6).unwrap();
    table.apply(&numbered(0, 2), applies).unwrap();
    let failed = table
        .apply(&numbered(2, 5), |mappings| {
            assert_eq!(targets(mappings), ["srv/2", "srv/3", "srv/4"]);
            Err((1, "the target is reserved"))
        })
        .unwrap_err();
    assert_eq!(
        failed,
        TableError::Failed {
            entry: 3,
            error: "the target is reserved",
        }
    );
    assert_eq!(failed.entry(), 3);
    assert_eq!(table.applied(), 3);
    assert!(table.is_latched() && !table.is_complete());
    // The table refuses every later part, wherever it continues, without mapping it.
    for (first, end) in [(3, 4), (5, 6), (3, 6)] {
        let refused = table
            .apply(&numbered(first, end), |_| -> Result<(), (usize, &str)> {
                panic!("a latched table maps nothing")
            })
            .unwrap_err();
        assert_eq!(
            refused,
            TableError::Refused {
                entry: 3,
                refusal: Refusal::Latched,
            }
        );
    }
    assert_eq!(table.applied(), 3);
}

#[test]
fn a_failure_counts_at_most_the_mappings_of_its_part() {
    // A root that fails before its first mapping, as one that cannot open its root does,
    // applies none; one that reports an index past the part's last applies them all.
    for (index, applied) in [(0, 1), (2, 3), (usize::MAX, 3)] {
        let mut table = MappingTable::new(4).unwrap();
        table.apply(&numbered(0, 1), applies).unwrap();
        let failed = table
            .apply(&numbered(1, 3), |_| Err((index, "failed")))
            .unwrap_err();
        assert_eq!(failed.entry(), applied, "{index}");
        assert_eq!(table.applied(), applied, "{index}");
        assert!(table.is_latched() && !table.is_complete(), "{index}");
    }
}

#[test]
fn a_latched_table_refuses_every_part() {
    // A caller latches the table once it refused a part, if its protocol ends the table then.
    let mut table = MappingTable::new(2).unwrap();
    let refused = table.apply(&numbered(1, 2), applies).unwrap_err();
    assert_eq!(refused.entry(), 0);
    table.latch();
    assert!(table.is_latched() && !table.is_complete());
    assert_eq!(
        table.apply(&numbered(0, 2), applies),
        Err(TableError::Refused {
            entry: 0,
            refusal: Refusal::Latched,
        })
    );
    // So does a complete table, which is complete no more.
    let mut table = MappingTable::new(0).unwrap();
    table.latch();
    assert!(!table.is_complete());
}

#[test]
fn every_entry_that_the_decoder_accepts_but_the_export_s_root_becomes_a_mapping() {
    assert_eq!(super::super::MAX_PATH_BYTES, protocol::MAX_MAP_PATH_BYTES);
    let longest = vec![b'a'; protocol::MAX_MAP_PATH_BYTES];
    let target = [b"/".as_slice(), &longest[1..]].concat();
    let payload = part(
        0,
        &[
            (longest.as_slice(), target.as_slice(), MAP_READ_ONLY),
            (b"0/\xff", b"/\xfe", 0),
        ],
        None,
    );
    let mut table = MappingTable::new(2).unwrap();
    table
        .apply(&payload, |mappings| {
            assert_eq!(mappings[0].source(), Path::new(OsStr::from_bytes(&longest)));
            assert_eq!(
                mappings[0].target(),
                Path::new(OsStr::from_bytes(&target[1..]))
            );
            applies(mappings)
        })
        .unwrap();
    assert!(table.is_complete());
}

#[test]
fn errors_say_which_entry_and_why() {
    let refused: TableError<&str> = TableError::Refused {
        entry: 2,
        refusal: Refusal::Order {
            first: 0,
            expected: 2,
        },
    };
    assert_eq!(
        refused.to_string(),
        "the mapping table refused a part: it starts at mapping 0, but the table expects \
             mapping 2 next"
    );
    let failed = TableError::Failed {
        entry: 3,
        error: "the target is reserved",
    };
    assert_eq!(
        failed.to_string(),
        "mapping 3 of the table failed: the target is reserved"
    );
}
