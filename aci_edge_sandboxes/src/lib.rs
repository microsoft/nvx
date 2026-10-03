//! Rust interface to the ACI Edge Sandboxes lifecycle.
//!
//! This crate exposes the five operations of the state-aware sandbox lifecycle:
//!
//! | Operation | Transition | Result |
//! | --- | --- | --- |
//! | [`AciEdgeSandbox::provision`] | (none) → provisioned | opaque [`SandboxId`] and optional metadata |
//! | [`AciEdgeSandbox::start`] | provisioned → running | optional metadata |
//! | [`AciEdgeSandbox::exec`] | running → running | live output streams and an [`ExecOutcome`] |
//! | [`AciEdgeSandbox::stop`] | running → provisioned | optional metadata |
//! | [`AciEdgeSandbox::deprovision`] | provisioned → (none) | optional metadata; the ID becomes stale |
//!
//! Every operation is implemented by a pluggable [`Backend`]. The default backend,
//! [`openvmm::OpenVmmBackend`], drives the `openvmm` binary directly. [`AciEdgeSandbox`] validates each
//! request in a fixed order before the backend acts on it: structural errors
//! ([`ErrorCode::MalformedRequest`], [`ErrorCode::MalformedId`]) come first, then requests the
//! backend cannot honor ([`ErrorCode::PolicyValidation`]), then backend-specific failures.
//! Rejected requests never run anything.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "openvmm")]
//! # fn main() -> aci_edge_sandboxes::Result<()> {
//! use aci_edge_sandboxes::openvmm::OpenVmmConfig;
//! use aci_edge_sandboxes::{AciEdgeSandbox, ExecRequest, ProvisionRequest};
//!
//! // Locates OpenVMM, the guest kernel, and the Alpine initramfs; see `openvmm::Artifacts`.
//! let client = AciEdgeSandbox::openvmm(OpenVmmConfig::discover()?)?;
//! let sandbox = client.provision(&ProvisionRequest::new())?.sandbox_id;
//! client.start(&sandbox)?;
//! let output = client
//!     .exec(&sandbox, &ExecRequest::command_line("echo hello"))?
//!     .wait_with_output()?;
//! assert_eq!(output.stdout, b"hello\n");
//! client.stop(&sandbox)?;
//! client.deprovision(&sandbox)?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "openvmm"))]
//! # fn main() {}
//! ```
//!
//! # Features
//!
//! - `openvmm` (default): the [`openvmm`] backend.
//! - `bundled`: stages the OpenVMM executable, guest kernel, and control initramfs at build time;
//!   see `openvmm::Artifacts::bundled`.
//! - `async`: Tokio wrappers ([`AsyncAciEdgeSandbox`], [`AsyncExecution`]) around the synchronous core.
//! - `testing`: an in-memory [`testing::MockBackend`] for consumers' own tests.

mod backend;
mod capabilities;
mod cidr;
mod client;
mod error;
mod exec;
mod id;
mod input;
mod model;
mod stream;
mod validate;

#[cfg(feature = "async")]
mod async_api;
#[cfg(feature = "openvmm")]
pub mod openvmm;
#[cfg(feature = "testing")]
pub mod testing;

#[cfg(feature = "async")]
pub use async_api::{AsyncAciEdgeSandbox, AsyncExecution, InputStream, OutputStream};
pub use backend::{Backend, ExecControl, ExecIo, OutputCloser, OutputSink};
pub use capabilities::{
    Capabilities, ExecCapabilities, FilesystemCapabilities, NetworkCapabilities,
};
pub use client::AciEdgeSandbox;
pub use error::{Error, ErrorBody, ErrorCode, Result};
pub use exec::{Canceller, ExecFailure, ExecOutcome, ExecOutput, Execution};
pub use id::SandboxId;
pub use input::{InputCloser, InputSource};
pub use model::{
    Access, Command, DeprovisionResult, EgressPolicy, ExecRequest, FilesystemPolicy, IngressPolicy,
    Metadata, MicrovmConfig, MicrovmProvision, NetworkPeer, NetworkPolicy, NetworkPort,
    NetworkRule, ProcessSpec, Protocol, ProvisionRequest, ProvisionResult, StartResult, StdinMode,
    StopResult,
};
