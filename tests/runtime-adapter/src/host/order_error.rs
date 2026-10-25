//! How the [host's order](super#the-hosts-order) of a VM with a layered root fails, which
//! [`ready`](super::ready), [`activate`](super::activate), [`map_table`](super::map_table), and
//! [`start_sandbox`](super::start_sandbox) return: an [`OrderError`].

use std::fmt;

use super::ClientError;
use crate::frame::BodyError;
use crate::root::{MapFailure, RootFailure};

/// A failure of the [host's order](super#the-hosts-order) of a VM with a layered root: what the
/// guest agent reported, or how the client failed, and what the host did about it.
#[derive(Debug)]
#[non_exhaustive]
pub enum OrderError {
    /// The guest agent reported that a stage of the root failed, after which it ends.
    Root(RootFailure),
    /// The guest agent assembled the root, or activated a clone, but is of another build than the
    /// one that the host ships, so the host sent it nothing more.
    Build {
        /// The build that the guest agent reported in its `Ready`.
        reported: String,
        /// The build that the host ships.
        expected: String,
    },
    /// The guest agent failed the mapping table, so the host sent it no more of the table, nor
    /// `Start`, and asked it to stop, which tears its root down.
    Mapping {
        /// How the table failed.
        failure: MapFailure,
        /// How the stop ended, as [`Client::stop`](super::Client::stop) returns it: the failure
        /// of the root's teardown, if the guest reported one before it confirmed the stop.
        stop: Result<Option<RootFailure>, ClientError>,
    },
    /// The mapping table does not encode, so the host sent nothing of it.
    Table(BodyError),
    /// The client failed, which ended it.
    Client(ClientError),
}

impl fmt::Display for OrderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(failure) => failure.fmt(formatter),
            Self::Build { reported, expected } => write!(
                formatter,
                "the guest agent is of build {reported}, not of the host's build {expected}"
            ),
            Self::Mapping { failure, stop } => {
                failure.fmt(formatter)?;
                match stop {
                    Ok(None) => Ok(()),
                    Ok(Some(teardown)) => write!(formatter, "; then {teardown}"),
                    Err(error) => {
                        write!(
                            formatter,
                            "; the guest agent did not confirm the stop: {error}"
                        )
                    }
                }
            }
            Self::Table(error) => error.fmt(formatter),
            Self::Client(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OrderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::Client(error) => Some(error),
            Self::Root(_) | Self::Build { .. } | Self::Mapping { .. } => None,
        }
    }
}
