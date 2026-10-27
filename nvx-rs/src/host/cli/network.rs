//! The network options that `nvx-rs sandbox` and `nvx-rs run` share with `nvx.py`, ported from
//! the parsers of `scripts/nvx.py`, `_resolve_network_egress_rules`, and
//! `_extend_network_arguments`: their values, the egress rules of a
//! `--network-egress-policy-file`, and the OpenVMM options that they become.

use std::path::Path;

use super::args::choice;
use super::{Exit, Result};
use crate::ErrorCode;
use crate::host::network::policy::compile_policy_file;

/// The choices of `--network-profile`.
const NETWORK_PROFILES: [&str; 1] = ["portable"];
/// The choices of `--network-egress`, `--network-ingress`, and `--host-loopback`.
const ACCESS: [&str; 2] = ["allow", "deny"];

/// The network options of a launch, as given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NetworkOptions {
    /// `--net`.
    pub(crate) net: Option<String>,
    /// `--network-profile`.
    pub(crate) network_profile: Option<&'static str>,
    /// `--network-egress`.
    pub(crate) network_egress: Option<&'static str>,
    /// `--network-ingress`.
    pub(crate) network_ingress: Option<&'static str>,
    /// The `--network-egress-allow` options.
    pub(crate) network_egress_allow: Vec<String>,
    /// The `--network-egress-deny` options.
    pub(crate) network_egress_deny: Vec<String>,
    /// `--network-egress-policy-file`, as given.
    pub(crate) network_egress_policy_file: Option<String>,
    /// `--host-loopback`.
    pub(crate) host_loopback: Option<&'static str>,
    /// `--network-proxy`.
    pub(crate) network_proxy: Option<String>,
    /// The `--host-loopback-forward` options.
    pub(crate) host_loopback_forward: Vec<String>,
}

/// The rules of the egress policy of a launch, in OpenVMM's native form.
pub(crate) struct NativeRules {
    /// The `--network-egress-allow` rules.
    pub(crate) allow: Vec<String>,
    /// The `--network-egress-deny` rules.
    pub(crate) deny: Vec<String>,
}

impl NetworkOptions {
    /// Takes `value` for the network option `name`, checking the choices of those that have
    /// them, as a parser's [`Receiver`](super::args::Receiver) does.
    ///
    /// # Panics
    ///
    /// If `name` is not a network option, which the parsers never pass.
    pub(crate) fn take(&mut self, name: &str, value: &str) -> std::result::Result<(), String> {
        let text = || value.to_owned();
        match name {
            "--net" => self.net = Some(text()),
            "--network-profile" => self.network_profile = Some(choice(value, &NETWORK_PROFILES)?),
            "--network-egress" => self.network_egress = Some(choice(value, &ACCESS)?),
            "--network-ingress" => self.network_ingress = Some(choice(value, &ACCESS)?),
            "--network-egress-allow" => self.network_egress_allow.push(text()),
            "--network-egress-deny" => self.network_egress_deny.push(text()),
            "--network-egress-policy-file" => self.network_egress_policy_file = Some(text()),
            "--host-loopback" => self.host_loopback = Some(choice(value, &ACCESS)?),
            "--network-proxy" => self.network_proxy = Some(text()),
            "--host-loopback-forward" => self.host_loopback_forward.push(text()),
            _ => unreachable!("the parsers pass only their options"),
        }
        Ok(())
    }

    /// Returns whether any network option was given.
    pub(crate) fn any(&self) -> bool {
        self.net.is_some()
            || self.network_profile.is_some()
            || self.network_egress.is_some()
            || self.network_ingress.is_some()
            || self.network_egress_policy_file.is_some()
            || self.host_loopback.is_some()
            || self.network_proxy.is_some()
            || !self.network_egress_allow.is_empty()
            || !self.network_egress_deny.is_empty()
            || !self.host_loopback_forward.is_empty()
    }

    /// Returns `_resolve_network_egress_rules(args)`: the explicit rules, or those that the
    /// `--network-egress-policy-file` compiles to, which neither rule option nor a missing
    /// `--network-egress` may accompany.
    pub(crate) fn egress_rules(&self) -> Result<NativeRules> {
        let Some(policy) = &self.network_egress_policy_file else {
            return Ok(NativeRules {
                allow: self.network_egress_allow.clone(),
                deny: self.network_egress_deny.clone(),
            });
        };
        if !self.network_egress_allow.is_empty() || !self.network_egress_deny.is_empty() {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "--network-egress-policy-file cannot be combined with --network-egress-allow or \
                 --network-egress-deny",
            ));
        }
        if self.network_egress.is_none() {
            return Err(Exit::error(
                ErrorCode::Unsupported,
                "--network-egress is required with --network-egress-policy-file",
            ));
        }
        let compiled = compile_policy_file(Path::new(policy))?;
        Ok(NativeRules {
            allow: compiled.allow,
            deny: compiled.deny,
        })
    }

    /// Appends the OpenVMM options of these network options and `rules` to `command`, as
    /// `_extend_network_arguments` does, for `run`, except that `--network-profile` goes without
    /// `--net` too, as a restore passes it; `sandbox` lowers them through its typed network policy
    /// instead.
    pub(crate) fn extend_arguments(&self, command: &mut Vec<String>, rules: &NativeRules) {
        let mut push = |name: &str, value: &str| {
            command.push(name.to_owned());
            command.push(value.to_owned());
        };
        if let Some(net) = &self.net {
            push("--net", net);
        }
        if let Some(profile) = self.network_profile {
            push("--network-profile", profile);
        }
        if let Some(egress) = self.network_egress {
            push("--network-egress", egress);
        }
        if let Some(ingress) = self.network_ingress {
            push("--network-ingress", ingress);
        }
        for rule in &rules.allow {
            push("--network-egress-allow", rule);
        }
        for rule in &rules.deny {
            push("--network-egress-deny", rule);
        }
        if let Some(host_loopback) = self.host_loopback {
            push("--host-loopback", host_loopback);
        }
        if let Some(proxy) = &self.network_proxy {
            push("--network-proxy", proxy);
        }
        for forward in &self.host_loopback_forward {
            push("--host-loopback-forward", forward);
        }
    }
}
