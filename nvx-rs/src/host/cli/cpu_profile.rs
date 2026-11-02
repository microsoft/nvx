//! The CPU-profile guidance that `nvx-rs run` prints after OpenVMM fails a cold boot on a host
//! whose CPU no built-in CPU profile serves.
//!
//! OpenVMM presents each guest a CPU profile, by default the built-in profile of the host CPU's
//! generation (`doc/usage.md`, "CPU profiles"). `cpu_generations.json` copies the identity of
//! every profile that OpenVMM pins, from its document in `vmm_core/cpu_profile/profiles/` of the
//! OpenVMM submodule: its ID, the vendor of its CPUs, and its generation's name and CPU models,
//! each a display family, a display model, and the first and last of its steppings. The revisions
//! of a generation's profile share its generation, which the guidance lists once.
//! `tests/openvmm_cpu_profiles.rs` compares the copy with the submodule's documents.
//! [`HostCpu::current`] identifies the host CPU with CPUID, as OpenVMM does.
//!
//! On an Intel or AMD CPU that no built-in profile serves, `--cpu-profile auto`, the default, falls
//! back to a profile derived from the host, which OpenVMM warns about, and on another vendor's
//! CPU, `auto` and `host` fail with `E_PROFILE_HOST_UNKNOWN`. OpenVMM keeps its standard error on
//! the terminal, so `nvx-rs run` cannot read the code that ended a cold boot: [`guidance`] says
//! what `auto` does on such a CPU, and on an Intel or AMD CPU, asks for the CPU's fingerprint,
//! from which NVX can add a built-in profile for it. An explicit profile ID fails with its own
//! code, and `host` on an Intel or AMD CPU with OpenVMM's own explanation, which need none.

use serde::Deserialize;

/// The identities of the built-in CPU profiles, in the order in which [`guidance`] lists their
/// generations.
const CATALOG: &str = include_str!("cpu_generations.json");
/// The `--cpu-profile` of a profile that OpenVMM derives from the host that it runs on.
const HOST_PROFILE: &str = "host";
/// The code with which OpenVMM fails a cold boot where no CPU profile serves the host's CPU.
const PROFILE_HOST_UNKNOWN: &str = "E_PROFILE_HOST_UNKNOWN";
/// The marker that leads OpenVMM's warning of each cold boot whose `--cpu-profile auto` falls back
/// to a host profile.
const FALLBACK_MARKER: &str = "NVX-CPU-PROFILE-FALLBACK:";
/// NVX's issue form that requests a built-in CPU profile for a CPU.
const REQUEST_URL: &str = "https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml";
/// The vendors whose CPUs host profiles serve, by their CPUID vendor strings, each with its name
/// and the issue that tracks built-in profiles for more of its CPUs.
const HOST_PROFILE_VENDORS: [(&str, &str, &str); 2] = [
    (
        "GenuineIntel",
        "Intel",
        "https://github.com/microsoft/nvx/issues/408",
    ),
    (
        "AuthenticAMD",
        "AMD",
        "https://github.com/microsoft/nvx/issues/409",
    ),
];

/// A CPU model of a generation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CpuModel {
    /// The display family.
    family: u32,
    /// The display model.
    model: u32,
    /// The first and the last stepping that the generation covers.
    steppings: [u32; 2],
}

impl CpuModel {
    /// Returns the model as the guidance names it: `6/106`, or `6/85 steppings 0-4` if the
    /// generation covers only some of its steppings.
    fn describe(&self) -> String {
        let name = format!("{}/{}", self.family, self.model);
        match self.steppings {
            [0, 15] => name,
            [first, last] => format!("{name} steppings {first}-{last}"),
        }
    }
}

/// The CPU generation that a built-in profile serves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    /// The generation's name, such as `skylake-sp`.
    name: String,
    /// The generation's CPU models.
    cpus: Vec<CpuModel>,
}

/// The identity of a built-in CPU profile.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    /// The profile's ID, such as `intel.skylake-sp.v1`.
    id: String,
    /// The CPUID vendor string of the generation's CPUs, such as `GenuineIntel`.
    vendor: String,
    /// The generation that the profile serves.
    generation: Generation,
}

impl Profile {
    /// Returns whether the profile serves `cpu`.
    fn serves(&self, cpu: &HostCpu) -> bool {
        self.vendor == cpu.vendor
            && self.generation.cpus.iter().any(|model| {
                (model.family, model.model) == (cpu.family, cpu.model)
                    && (model.steppings[0]..=model.steppings[1]).contains(&cpu.stepping)
            })
    }

    /// Returns the profile's generation and its CPU models, such as `alderlake 6/151 and 6/154`.
    fn describe(&self) -> String {
        let cpus: Vec<String> = self
            .generation
            .cpus
            .iter()
            .map(CpuModel::describe)
            .collect();
        format!("{} {}", self.generation.name, cpus.join(" and "))
    }
}

/// Returns the built-in CPU profiles of [`CATALOG`].
fn catalog() -> Vec<Profile> {
    serde_json::from_str(CATALOG).expect("cpu_generations.json lists the built-in CPU profiles")
}

/// Returns the generations of the built-in profiles in `catalog` as the guidance lists them, such
/// as `milan 25/1, genoa 25/17`: each once, in the order of its first profile, since the revisions
/// of a generation's profile share it.
fn describe_generations(catalog: &[Profile]) -> String {
    let mut generations: Vec<String> = Vec::new();
    for generation in catalog.iter().map(Profile::describe) {
        if !generations.contains(&generation) {
            generations.push(generation);
        }
    }
    generations.join(", ")
}

/// A host CPU, as CPUID identifies it. Only x86-64 hosts have CPUID, so elsewhere, nothing
/// identifies one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(crate) struct HostCpu {
    /// The vendor string of CPUID leaf 0, such as `GenuineIntel`.
    vendor: String,
    /// The display family.
    family: u32,
    /// The display model.
    model: u32,
    /// The stepping.
    stepping: u32,
    /// The brand string of CPUID leaves 0x80000002 to 0x80000004, or nothing.
    brand: String,
}

impl HostCpu {
    /// Identifies the CPU that this process runs on, or returns `None` on a host without CPUID,
    /// which is not an x86-64 host.
    pub(crate) fn current() -> Option<Self> {
        #[cfg(target_arch = "x86_64")]
        {
            Some(Self::from_cpuid(cpuid))
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            None
        }
    }

    /// Identifies the CPU from `cpuid`, which returns EAX, EBX, ECX, and EDX of a leaf, as
    /// OpenVMM's `cpu_profile` crate identifies it: Intel extends the model for families 6 and 15,
    /// and AMD only for family 15.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    fn from_cpuid(mut cpuid: impl FnMut(u32) -> [u32; 4]) -> Self {
        let [max_basic, ebx, ecx, edx] = cpuid(0);
        let vendor = register_text(&[ebx, edx, ecx]);
        let signature = if max_basic >= 1 { cpuid(1)[0] } else { 0 };
        let brand = if cpuid(0x8000_0000)[0] >= 0x8000_0004 {
            let registers: Vec<u32> = (0x8000_0002..=0x8000_0004).flat_map(&mut cpuid).collect();
            register_text(&registers)
        } else {
            String::new()
        };
        let stepping = signature & 0xf;
        let base_model = (signature >> 4) & 0xf;
        let base_family = (signature >> 8) & 0xf;
        let family = if base_family == 0xf {
            base_family + ((signature >> 20) & 0xff)
        } else {
            base_family
        };
        let extended = base_family == 0xf || (base_family == 6 && vendor == "GenuineIntel");
        let model = if extended {
            (((signature >> 16) & 0xf) << 4) | base_model
        } else {
            base_model
        };
        Self {
            vendor,
            family,
            model,
            stepping,
            brand,
        }
    }

    /// Returns the CPU's vendor and display family, model, and stepping, such as
    /// `GenuineIntel 6/173/1`.
    fn signature(&self) -> String {
        format!(
            "{} {}/{}/{}",
            self.vendor, self.family, self.model, self.stepping
        )
    }

    /// Returns the CPU as the guidance names it: its signature and its brand string, if any, such
    /// as `GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C)`.
    fn describe(&self) -> String {
        if self.brand.is_empty() {
            self.signature()
        } else {
            format!("{} ({})", self.signature(), self.brand)
        }
    }

    /// Returns the link to the CPU profile request form for the CPU, which prefills the form's
    /// title and CPU fields, as OpenVMM's fallback warning does.
    fn request_url(&self) -> String {
        let mut url = format!(
            "{REQUEST_URL}&title={}&signature={}",
            encode_query_value(&format!("CPU profile: {}", self.describe())),
            encode_query_value(&self.signature())
        );
        if !self.brand.is_empty() {
            url.push_str("&cpu=");
            url.push_str(&encode_query_value(&self.brand));
        }
        url
    }
}

/// Runs CPUID with `leaf` and subleaf 0, and returns EAX, EBX, ECX, and EDX.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code, unused_unsafe)]
fn cpuid(leaf: u32) -> [u32; 4] {
    // SAFETY: CPUID executes in user mode on every x86-64 processor and accesses no memory. The
    // intrinsic became a safe function after the crate's minimum Rust version.
    let result = unsafe { core::arch::x86_64::__cpuid_count(leaf, 0) };
    [result.eax, result.ebx, result.ecx, result.edx]
}

/// Returns the text of the little-endian bytes of `registers`, as CPUID's vendor and brand
/// strings hold it, without the NULs that pad it or the spaces around it.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn register_text(registers: &[u32]) -> String {
    let bytes: Vec<u8> = registers
        .iter()
        .flat_map(|register| register.to_le_bytes())
        .collect();
    String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .trim()
        .to_owned()
}

/// Percent-encodes `value` for a URL query, as OpenVMM's fallback warning does: every byte but
/// ASCII letters, digits, `-`, `.`, `_`, and `~`.
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Returns what to print after OpenVMM failed a cold boot with `--cpu-profile cpu_profile`, or
/// with its default, `auto`, if `None`, on the CPU `host` with the `hypervisor` backend, where
/// `openvmm` is the OpenVMM executable, quoted for the shell. Returns `None` unless no built-in
/// profile serves the CPU and the profile is `auto`, or `host` on a CPU of a vendor that host
/// profiles do not serve either, and where the CPU is unknown.
pub(crate) fn guidance(
    cpu_profile: Option<&str>,
    host: Option<&HostCpu>,
    openvmm: &str,
    hypervisor: &str,
) -> Option<String> {
    let host = host?;
    let catalog = catalog();
    if catalog.iter().any(|profile| profile.serves(host)) {
        return None;
    }
    let vendor = HOST_PROFILE_VENDORS
        .iter()
        .find(|(vendor, ..)| *vendor == host.vendor);
    match cpu_profile {
        None | Some("auto") => {}
        Some(HOST_PROFILE) if vendor.is_none() => {}
        Some(_) => return None,
    }
    let unserved = format!(
        "nvx: no built-in CPU profile serves this host's CPU, {}",
        host.describe()
    );
    let generations = format!(
        "the built-in profiles cover {}.",
        describe_generations(&catalog)
    );
    let Some((_, name, issue)) = vendor else {
        return Some(format!(
            "{unserved}, so a cold boot with --cpu-profile auto, the default, fails on it with \
             {PROFILE_HOST_UNKNOWN}; {generations}\n\
             nvx: --cpu-profile {HOST_PROFILE} cannot boot on it either: host CPU profiles serve \
             only Intel and AMD CPUs."
        ));
    };
    Some(
        [
            format!(
                "{unserved}, so --cpu-profile auto, the default, falls back on it to a CPU profile \
                 derived from this host, which OpenVMM warns about with {FALLBACK_MARKER}; \
                 {generations}"
            ),
            "nvx: OpenVMM's error above names the cause of this failure, and says so where the \
             host profile cannot boot; doc/usage.md (\"CPU profiles\") explains a host profile's \
             limits."
                .to_owned(),
            "nvx: Help NVX add a built-in profile for this CPU: run".to_owned(),
            format!(
                "nvx:   {openvmm} --hypervisor {hypervisor} --cpu-fingerprint fingerprint.json"
            ),
            "nvx: and attach fingerprint.json to a CPU profile request:".to_owned(),
            format!("nvx:   {}", host.request_url()),
            format!("nvx: {issue} tracks CPU profiles for more {name} CPUs."),
        ]
        .join("\n"),
    )
}

#[cfg(test)]
mod tests;
