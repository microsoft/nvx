//! The activation record of a clone, which its [plan](super::ClonePlan) builds before the clone's
//! launch from the request and the checked generation: the record of the settings that the
//! clone's guest applies in place of the configuration that its template captured, as
//! [`nvx::activation`](crate::activation) describes it.
//!
//! A record names the bindings of the clone's image and scratch image, and the plan of its export,
//! by their identities: the SHA-256 digest of a domain of each kind of object and of the object's
//! serde form, which its callers record. The digest covers metadata alone, a few hundred bytes of
//! a binding, and the host paths and guest targets of an export plan, never the content of a
//! file.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::CloneRequest;
use super::failure::{Attribute, Evidence, Fault};
use crate::activation::{
    ActivationRecord, Configuration, ExpectedResources, Export, ExportKind, Identity, Image,
    Network, Publication, RestoreSection, TransactionId, Upper,
};
use crate::error::{Error, Result};
use crate::host::filesystem::ExportPlan;
use crate::host::sandbox::distro::{DistroBinding, DistroFormat};
use crate::host::sandbox::scratch::ScratchBinding;
use crate::host::template::key::TemplateKey;
use crate::host::template::store::Lease;

/// What the identity of an image's binding digests before the binding's serde form: a domain
/// that no other digest of this crate shares, and the NUL that ends it.
const DISTRO_DOMAIN: &[u8] = b"nvx distro binding\0";

/// What the identity of a scratch image's binding digests before the binding's serde form, as
/// [`DISTRO_DOMAIN`] does for an image's.
const SCRATCH_DOMAIN: &[u8] = b"nvx scratch binding\0";

/// What the identity of an export plan digests before the plan's serde form, as
/// [`DISTRO_DOMAIN`] does for an image's binding.
const EXPORT_DOMAIN: &[u8] = b"nvx export plan\0";

/// Returns the activation record of the clone that `request` asks for, of the template whose key
/// is `key`, which `lease` leases, with a fresh transaction ID.
///
/// The record holds:
///
/// - the restore section: a transaction ID of 16 random bytes, and the template that `lease`
///   names, its generation and the identity of its key;
/// - the image, in the format that its binding names: an EROFS layer, with the identity of its
///   binding and the UUID of its filesystem, or a GPT disk, with the identity of its binding, the
///   GUID of the disk, and the request's bound of its first partition; the adapter's plan names
///   the UUID or the GUID, as the binding does;
/// - the upper: a tmpfs, or the scratch, with the identity of its binding and the request's
///   journal policy;
/// - the request's overlay mode and workload view;
/// - the export, if the request has one: a directory, which stage 1's rule allows a clone alone,
///   with the identity of its plan, and the number of its mappings, the plan's binds, from 1 to
///   [`MAX_MAPPINGS`](crate::activation::MAX_MAPPINGS); or no export, and no mappings;
/// - the network of the template's NIC, if it has one: the identity of the policy that OpenVMM
///   enforces there, the digest that OpenVMM computed and the template's key records; the DNS
///   server with which the template's guest configured its resolver at boot, which the template's
///   command line names, [`TemplateKey::dns_server`], if it names one; unpublished listeners,
///   since a template's policy allows neither ingress nor host-loopback access both ways; and the
///   URL of the policy's proxy as the guest sees it, if it has one;
/// - the expected resources: the request's processor target, which the restore brings online, and
///   its memory target.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a configuration that
/// no record holds, as [`Configuration::check`] finds it, such as a GPT disk's first-partition
/// bound of 0, a hostname that is empty or longer than
/// [`MAX_HOSTNAME_BYTES`](crate::activation::MAX_HOSTNAME_BYTES), or a workload identity whose
/// UID or GID is 0, [`Evidence::InvalidActivate`]; with [`ErrorKind::Io`](crate::ErrorKind::Io)
/// for a key whose command line names its DNS server in a form that OpenVMM does not write, as
/// [`TemplateKey::dns_server`] finds it, [`Evidence::MalformedGeneration`]; and with
/// [`ErrorKind::Other`](crate::ErrorKind::Other) if the transaction ID cannot be drawn, or the
/// identity of a binding or of the export's plan computed, the host's evidence.
pub(super) fn build(
    request: &CloneRequest,
    lease: &Lease,
    key: &TemplateKey,
) -> std::result::Result<ActivationRecord, Fault> {
    let binding = distro_identity(&request.binding).on_host()?;
    let expected = request.adapter.distro().image().identity;
    let image = match request.binding.format() {
        DistroFormat::Erofs => Image::Erofs {
            binding,
            uuid: expected,
        },
        DistroFormat::Gpt => Image::Gpt {
            binding,
            disk_guid: expected,
            first_partition_bound: request.first_partition_bound,
        },
    };
    let upper = match &request.scratch {
        None => Upper::Tmpfs,
        Some(binding) => Upper::Scratch {
            binding: scratch_identity(binding).on_host()?,
            journal: request.journal,
        },
    };
    let dns = key.dns_server().attribute(Evidence::MalformedGeneration)?;
    let network = key.network_policy_digest().map(|policy| Network {
        policy: Identity::from_bytes(policy),
        dns,
        publication: Publication::Unpublished,
        proxy: request
            .adapter
            .network()
            .and_then(|network| network.proxy_url()),
    });
    let (export, mappings) = match &request.export {
        None => (None, 0),
        Some(plan) => (
            Some(Export {
                kind: ExportKind::Directory,
                plan: export_identity(plan).on_host()?,
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
        workload: request.workload.clone(),
        resources: ExpectedResources {
            cpus: request.processors,
            memory: request.memory,
        },
    };
    let restore = RestoreSection {
        transaction: transaction().on_host()?,
        template: lease.template(),
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

/// Returns the identity of `binding`, the binding of a clone's image, as the [module](self)
/// describes it.
pub(super) fn distro_identity(binding: &DistroBinding) -> Result<Identity> {
    identity(DISTRO_DOMAIN, binding, "binding of the clone's image")
}

/// Returns the identity of `binding`, the binding of a clone's scratch image, as the
/// [module](self) describes it.
pub(super) fn scratch_identity(binding: &ScratchBinding) -> Result<Identity> {
    identity(
        SCRATCH_DOMAIN,
        binding,
        "binding of the clone's scratch image",
    )
}

/// Returns the identity of `plan`, the plan of a clone's export, as the [module](self) describes
/// it: it covers the export's whole shape, its directories and files, their policies, the binds,
/// and the owner.
pub(super) fn export_identity(plan: &ExportPlan) -> Result<Identity> {
    identity(EXPORT_DOMAIN, plan, "plan of the clone's export")
}

/// Returns the SHA-256 digest of `domain` and of the serde form of `object`, a clone's binding or
/// export plan, which messages call `what`.
fn identity(domain: &[u8], object: &impl Serialize, what: &str) -> Result<Identity> {
    let form = serde_json::to_vec(object).map_err(|error| {
        Error::other(format!("cannot write the {what} in its serde form")).with_source(error)
    })?;
    let digest = Sha256::new()
        .chain_update(domain)
        .chain_update(form)
        .finalize();
    Ok(Identity::from_bytes(digest.into()))
}

/// Returns a fresh transaction ID: 16 random bytes.
fn transaction() -> Result<TransactionId> {
    let mut bytes = [0; TransactionId::LEN];
    getrandom::fill(&mut bytes).map_err(|error| {
        Error::other("cannot draw the transaction ID of a clone's restore").with_source(error)
    })?;
    Ok(TransactionId::from_bytes(bytes))
}
