//! The worker's order once its root is ready, as the [worker's documentation](super) describes:
//! it applies the host's mapping table, in parts, then starts the sandbox at the host's `Start`,
//! and only then runs the host's executions.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use nvx::guest::mappings::{Access, Mapping};
use nvx::protocol::GuestHostMap;

use super::RootOperations;
use crate::frame::Message;
use crate::root::{MapFailure, decode_maps};

/// Where the worker is in its order once its root is ready: how much of the host's mapping table
/// it applied, and whether the host started the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Order {
    /// The number of mappings that the root configuration announced.
    announced: u32,
    /// How many of them the worker applied, in order.
    applied: u32,
    /// Whether the worker answered a part of the table with `MapFailed`, after which it refuses
    /// every later part, and the start.
    failed: bool,
    /// Whether the host started the sandbox.
    started: bool,
}

impl Order {
    /// Returns the order of a root whose configuration announced `announced` mappings, none of
    /// which the worker applied yet.
    pub(super) fn new(announced: u32) -> Self {
        Self {
            announced,
            applied: 0,
            failed: false,
            started: false,
        }
    }

    /// Returns whether the host started the sandbox: only then does the worker run the host's
    /// executions, and it takes no part of the mapping table any more.
    pub(super) fn is_started(&self) -> bool {
        self.started
    }

    /// Returns whether the host may start the sandbox: once the worker applied every mapping that
    /// the root configuration announced, and none failed, until the host started it.
    pub(super) fn may_start(&self) -> bool {
        !self.started && !self.failed && self.applied == self.announced
    }

    /// Starts the sandbox, which the host [may start](Self::may_start), and returns the worker's
    /// confirmation, `Started`.
    pub(super) fn start(&mut self) -> Message {
        debug_assert!(self.may_start(), "the host may not start the sandbox");
        self.started = true;
        Message::Started
    }

    /// Applies the part of the mapping table `body`, from a `Maps` before the start, through
    /// `operations`, and returns the worker's answer: `Mapped` once it applied the part's
    /// mappings, or `MapFailed` once it refused the part, or failed to apply one of its mappings,
    /// after which it refuses every later part.
    pub(super) fn map(&mut self, operations: &mut dyn RootOperations, body: &[u8]) -> Message {
        match self.apply(operations, body) {
            Ok(()) => Message::Mapped,
            Err(failure) => {
                self.failed = true;
                Message::MapFailed(failure.encode())
            }
        }
    }

    /// Applies the part `body` as [`map`](Self::map) does, and returns the failure that its
    /// `MapFailed` reports, if any.
    ///
    /// It refuses a part as a whole, at the mapping that it expects next, once the table failed,
    /// and if the part does not decode, or does not continue the table: its first mapping is not
    /// the one that the worker expects next, or its last is past the count that the configuration
    /// announced. Otherwise the operations apply its mappings in order, and keep those before the
    /// first that fails, which the failure counts.
    fn apply(
        &mut self,
        operations: &mut dyn RootOperations,
        body: &[u8],
    ) -> Result<(), MapFailure> {
        let applied = self.applied;
        let refused = |problem: String| MapFailure::new(applied, problem);
        if self.failed {
            return Err(refused(
                "the mapping table failed before this part".to_owned(),
            ));
        }
        let part = decode_maps(body).map_err(|error| refused(error.problem().to_owned()))?;
        if part.first != applied {
            return Err(refused(format!(
                "a part of the mapping table starts at its mapping {}, but the worker expects its \
                 mapping {applied} next",
                part.first
            )));
        }
        // `decode_maps` ends a part within the largest table, of `MAX_HOST_MAPPINGS` mappings, so
        // its count fits a `u32`.
        let count = part.maps.len() as u32;
        let end = applied + count;
        if end > self.announced {
            return Err(refused(format!(
                "a part of the mapping table ends it at {end} mappings, past the {} that the root \
                 configuration announced",
                self.announced
            )));
        }
        let mappings = part
            .maps
            .into_iter()
            .map(mapping)
            .collect::<Result<Vec<_>, _>>()
            .map_err(refused)?;
        match operations.map(&mappings) {
            Ok(()) => {
                self.applied = end;
                Ok(())
            }
            Err(error) => {
                // The operations applied at most the part's mappings, whose count fits a `u32`.
                self.applied += error.applied.min(mappings.len()) as u32;
                Err(MapFailure::new(self.applied, error.problem))
            }
        }
    }
}

/// Returns the mapping of `map`, an entry of the host's mapping table, whose paths the guest takes
/// as bytes, as Linux does, from a source relative to the host export's root to a target in the
/// root, read-only if the entry says so, or the problem.
fn mapping(map: GuestHostMap) -> Result<Mapping, String> {
    let access = if map.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let source = PathBuf::from(OsString::from_vec(map.source));
    let target = PathBuf::from(OsString::from_vec(map.target));
    Mapping::new(source, target, access).map_err(|error| {
        format!("a part of the mapping table holds a mapping that the guest refuses: {error}")
    })
}
