//! The worker's order once its root is ready, as the [worker's documentation](super) describes:
//! it applies the host's mapping table, in parts, then starts the sandbox at the host's `Start`,
//! and only then runs the host's executions.

use nvx::guest::mappings::{MappingTable, TableError};

use super::RootOperations;
use crate::frame::Message;
use crate::root::MapFailure;

/// Where the worker is in its order once its root is ready: its account of the host's mapping
/// table, and whether the host started the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Order {
    /// The host's mapping table, which `nvx::guest::mappings::MappingTable` keeps: how many
    /// mappings the root configuration announced, how many of them the worker applied, in order,
    /// and whether the worker answered a part with `MapFailed`, which latches the table, after
    /// which the worker refuses every later part, and the start.
    table: MappingTable,
    /// Whether the host started the sandbox.
    started: bool,
}

impl Order {
    /// Returns the order of a root whose configuration announced `announced` mappings, none of
    /// which the worker applied yet.
    ///
    /// # Panics
    ///
    /// If `announced` is more than `nvx::protocol::MAX_HOST_MAPPINGS`, which no table holds, and
    /// which `RootConfig::decode` refuses.
    pub(super) fn new(announced: u32) -> Self {
        Self {
            table: MappingTable::new(announced)
                .expect("a root configuration announces no more mappings than a table holds"),
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
        !self.started && self.table.is_complete()
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
    ///
    /// The table, through `nvx::guest::mappings::MappingTable::apply`, refuses a part as a whole,
    /// at the mapping that it expects next, once the table failed, and if the part does not
    /// decode, holds no mapping, or does not continue the table: its first mapping is not the one
    /// that the worker expects next, or its last is past the count that the configuration
    /// announced. It also refuses a part with a mapping of the host export's root, which an empty
    /// source names, since the export is an aggregate whose root lists its children. Otherwise
    /// the operations apply its mappings in order, and keep those before the first that fails,
    /// which the failure counts. Either latches the table.
    pub(super) fn map(&mut self, operations: &mut dyn RootOperations, body: &[u8]) -> Message {
        let applied = self.table.apply(body, |mappings| {
            operations
                .map(mappings)
                .map_err(|error| (error.applied, error.problem))
        });
        let failure = match applied {
            Ok(()) => return Message::Mapped,
            Err(refused @ TableError::Refused { .. }) => {
                MapFailure::new(refused.entry(), refused.to_string())
            }
            Err(TableError::Failed { entry, error }) => MapFailure::new(entry, error),
        };
        // The worker starts no workload on a partial table, nor on one whose part it refused.
        self.table.latch();
        Message::MapFailed(failure.encode())
    }
}
