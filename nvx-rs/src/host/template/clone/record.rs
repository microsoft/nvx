//! The activation record of a clone, which its [plan](super::ClonePlan) builds before the clone's
//! launch from the request and the checked generation: the record of the settings that the
//! clone's guest applies in place of the configuration that its template captured, as
//! [`nvx::activation`](crate::activation) describes it.

use super::CloneRequest;
use super::failure::{Attribute, Evidence, Fault};
use crate::activation::{
    ActivationRecord, Configuration, ExpectedResources, Export, ExportKind, Image, RestoreSection,
    TransactionId, Upper,
};
use crate::error::{Error, Result};
use crate::host::config::UpperConfig;
use crate::host::sandbox::distro::DistroFormat;
use crate::host::template::key::TemplateKey;

/// Returns the activation record of the clone that `request` asks for, of the template whose key
/// is `key`, with a fresh transaction ID.
///
/// The record holds:
///
/// - the restore section: only a transaction ID of 16 random bytes;
/// - the image, in the format that its binding names: an EROFS layer with its filesystem UUID,
///   or a GPT disk with its GUID and the request's bound of its first partition;
/// - the upper: a tmpfs capped at half of the VM's memory, or the scratch with the request's
///   journal policy;
/// - the request's overlay mode and view;
/// - the export, if the request has one: an aggregate, as every export plan is, and the number
///   of its mappings, the plan's binds, from 1 to
///   [`MAX_MAPPINGS`](crate::activation::MAX_MAPPINGS); or no export, and no mappings;
/// - the clone's network, if the template has a NIC, whose policy the restore enforces there in
///   place of the template's complete deny-all policy: the DNS server that OpenVMM serves the
///   guest at a gateway under the
///   policy, as [`SandboxNetwork::dns_server`](crate::host::sandbox::SandboxNetwork::dns_server)
///   finds it with the IPv6 gateway of the template's
///   NIC, [`TemplateKey::ipv6_gateway`], if it serves one, since the template's guest configured
///   no resolver at boot under the complete deny-all policy; published listeners under a policy
///   that publishes them, [`NetworkPolicy::publishes_listeners`], which allows ingress or
///   host-loopback access both ways, and unpublished ones otherwise; and the URL of the policy's
///   proxy as the guest sees it, if it has one;
/// - the expected resources: the request's processor target, which the restore brings online, and
///   its memory target.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a configuration that
/// no record holds, as [`Configuration::check`] finds it, such as a GPT disk's first-partition
/// bound of 0, a hostname that is empty or longer than
/// [`MAX_HOSTNAME_BYTES`](crate::activation::MAX_HOSTNAME_BYTES), [`Evidence::InvalidActivate`],
/// and for a network whose policy does not lower
/// to OpenVMM's arguments, [`Evidence::InvalidSettings`]; and with
/// [`ErrorKind::Other`](crate::ErrorKind::Other) if the transaction ID cannot be drawn, the
/// host's evidence.
///
/// [`NetworkPolicy::publishes_listeners`]: crate::host::network::NetworkPolicy::publishes_listeners
pub(super) fn build(
    request: &CloneRequest,
    key: &TemplateKey,
) -> std::result::Result<ActivationRecord, Fault> {
    let expected = request.adapter.distro().image().identity;
    let image = match request.binding.format() {
        DistroFormat::Erofs => Image::Erofs { uuid: expected },
        DistroFormat::Gpt => Image::Gpt {
            disk_guid: expected,
            first_partition_bound: request.first_partition_bound,
        },
    };
    let upper = match &request.scratch {
        None => Upper::Tmpfs {
            max_memory_percent: UpperConfig::DEFAULT_MAX_MEMORY_PERCENT,
        },
        Some(_) => Upper::Scratch {
            journal: request.journal,
        },
    };
    // The plan's check found that the network attaches a NIC if and only if the template has one.
    let network = match request.adapter.network().filter(|_| key.has_network()) {
        None => None,
        Some(network) => Some(
            network.policy().activation_network(
                network.gateway(),
                network
                    .dns_server(key.ipv6_gateway())
                    .attribute(Evidence::InvalidSettings)?,
            ),
        ),
    };
    let (export, mappings) = match &request.export {
        None => (None, 0),
        Some(plan) => (
            Some(Export {
                kind: ExportKind::Aggregate,
            }),
            plan.binds.len(),
        ),
    };
    let configuration = Configuration {
        image,
        upper,
        overlay: request.overlay,
        export,
        mappings,
        network,
        view: request.view.clone(),
        resources: ExpectedResources {
            cpus: request.processors,
            memory: request.memory,
        },
    };
    let restore = RestoreSection {
        transaction: transaction().on_host()?,
    };
    ActivationRecord::new(Some(restore), configuration).map_err(|error| {
        Fault::new(
            Evidence::InvalidActivate,
            Error::unsupported(format!(
                "no clone activates with the settings of the request: {error}"
            )),
        )
    })
}

/// Returns a fresh transaction ID: 16 random bytes.
fn transaction() -> Result<TransactionId> {
    let mut bytes = [0; TransactionId::LEN];
    getrandom::fill(&mut bytes).map_err(|error| {
        Error::other("cannot draw the transaction ID of a clone's restore").with_source(error)
    })?;
    Ok(TransactionId::from_bytes(bytes))
}
