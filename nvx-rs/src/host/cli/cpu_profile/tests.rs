use super::*;

/// Returns the CPU of `vendor` with the display `family`, `model`, and `stepping`, and the
/// brand string `brand`.
fn cpu(vendor: &str, family: u32, model: u32, stepping: u32, brand: &str) -> HostCpu {
    HostCpu {
        vendor: vendor.to_owned(),
        family,
        model,
        stepping,
        brand: brand.to_owned(),
    }
}

/// Returns the guidance for `host` with `--cpu-profile cpu_profile`, for `/nvx/openvmm` on KVM.
fn guide(cpu_profile: Option<&str>, host: &HostCpu) -> Option<String> {
    guidance(cpu_profile, Some(host), "/nvx/openvmm", "kvm")
}

/// Returns the name of the generation that serves the CPU of `vendor` with `family`, `model`,
/// and `stepping`, if any.
fn generation(vendor: &str, family: u32, model: u32, stepping: u32) -> Option<String> {
    let host = cpu(vendor, family, model, stepping, "");
    catalog()
        .into_iter()
        .find(|profile| profile.serves(&host))
        .map(|profile| profile.generation.name)
}

#[test]
fn the_catalog_lists_the_built_in_profiles() {
    let catalog = catalog();
    let ids: Vec<&str> = catalog.iter().map(|profile| profile.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "intel.skylake-sp.v1",
            "intel.icelake-sp.v1",
            "intel.emeraldrapids.v1",
            "intel.alderlake.v1",
            "amd.milan.v1",
            "amd.genoa.v1",
            "amd.genoa.v2",
            "amd.turin.v1",
        ]
    );
    // The two revisions of Genoa's profile share its generation, which the guidance lists
    // once.
    assert_eq!(
        describe_generations(&catalog),
        "skylake-sp 6/85 steppings 0-4, icelake-sp 6/106, emeraldrapids 6/207, alderlake \
             6/151 and 6/154, milan 25/1, genoa 25/17, turin 26/2"
    );
}

#[test]
fn cpus_map_to_the_generations_of_the_built_in_profiles() {
    let intel = |model, stepping| generation("GenuineIntel", 6, model, stepping);
    assert_eq!(intel(85, 4).as_deref(), Some("skylake-sp"));
    assert_eq!(intel(106, 6).as_deref(), Some("icelake-sp"));
    assert_eq!(intel(207, 2).as_deref(), Some("emeraldrapids"));
    // Alder Lake-S and Alder Lake-P and -H share one profile.
    assert_eq!(intel(151, 2).as_deref(), Some("alderlake"));
    assert_eq!(intel(154, 3).as_deref(), Some("alderlake"));
    // Cascade Lake and Cooper Lake share model 85 but have no profile, and neither have
    // Sapphire Rapids, Tiger Lake, and Raptor Lake.
    for (model, stepping) in [(85, 7), (85, 11), (143, 8), (140, 1), (183, 1)] {
        assert_eq!(intel(model, stepping), None, "6/{model}/{stepping}");
    }
    // Milan's profile serves every stepping of AMD's family 25 model 1, Genoa's every stepping
    // of model 17, and Turin's every stepping of family 26 model 2.
    for (family, model, expected) in [(25, 1, "milan"), (25, 17, "genoa"), (26, 2, "turin")] {
        for stepping in [0, 1, 2, 15] {
            assert_eq!(
                generation("AuthenticAMD", family, model, stepping).as_deref(),
                Some(expected)
            );
        }
    }
    // Vermeer and Raphael (Ryzen), Turin Dense, and Granite Ridge have no profile, and neither
    // has another vendor's CPU with the signature of an AMD generation.
    for (vendor, family, model) in [
        ("AuthenticAMD", 1, 1),
        ("AuthenticAMD", 25, 33),
        ("AuthenticAMD", 25, 97),
        ("AuthenticAMD", 26, 17),
        ("AuthenticAMD", 26, 68),
        ("GenuineIntel", 25, 1),
        ("HygonGenuine", 25, 1),
        ("GenuineIntel", 26, 2),
    ] {
        assert_eq!(
            generation(vendor, family, model, 1),
            None,
            "{vendor} {family}/{model}"
        );
    }
}

/// Returns the registers of a CPUID leaf that hold `text`, as the vendor and brand leaves
/// hold it.
fn words(text: &[u8]) -> Vec<u32> {
    text.chunks(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

/// Returns the CPUID of a CPU of `vendor` with the CPUID.1:EAX `signature` and, if not empty,
/// the 48-byte `brand`.
fn fake_cpuid(vendor: &[u8; 12], signature: u32, brand: &[u8]) -> impl FnMut(u32) -> [u32; 4] {
    let vendor = words(vendor);
    let brand = words(brand);
    let max_extended = if brand.is_empty() {
        0x8000_0001
    } else {
        0x8000_0004
    };
    move |leaf| match leaf {
        0 => [0x1b, vendor[0], vendor[2], vendor[1]],
        1 => [signature, 0, 0, 0],
        0x8000_0000 => [max_extended, 0, 0, 0],
        0x8000_0002..=0x8000_0004 if !brand.is_empty() => {
            let first = (leaf - 0x8000_0002) as usize * 4;
            [
                brand[first],
                brand[first + 1],
                brand[first + 2],
                brand[first + 3],
            ]
        }
        _ => [0; 4],
    }
}

#[test]
fn cpuid_identifies_the_cpu_as_openvmm_does() {
    let skylake = HostCpu::from_cpuid(fake_cpuid(
        b"GenuineIntel",
        0x0005_0654,
        b"Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz\0\0\0\0\0\0",
    ));
    assert_eq!(
        skylake,
        cpu(
            "GenuineIntel",
            6,
            85,
            4,
            "Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz"
        )
    );
    // Older Intel brand strings start with spaces, which are trimmed as NULs are.
    let padded = HostCpu::from_cpuid(fake_cpuid(
        b"GenuineIntel",
        0x000c_06f2,
        b"        Intel(R) Xeon(R) Platinum 8573C\0\0\0\0\0\0\0\0\0",
    ));
    assert_eq!((padded.family, padded.model, padded.stepping), (6, 207, 2));
    assert_eq!(padded.brand, "Intel(R) Xeon(R) Platinum 8573C");
    // AMD extends the family and the model of family 15 alone.
    let genoa = HostCpu::from_cpuid(fake_cpuid(b"AuthenticAMD", 0x00a1_0f11, b""));
    assert_eq!(genoa, cpu("AuthenticAMD", 25, 17, 1, ""));
    let k7 = HostCpu::from_cpuid(fake_cpuid(b"AuthenticAMD", 0x0001_0661, b""));
    assert_eq!((k7.family, k7.model, k7.stepping), (6, 6, 1));
    let p6 = HostCpu::from_cpuid(fake_cpuid(b"GenuineIntel", 0x0001_0661, b""));
    assert_eq!((p6.family, p6.model, p6.stepping), (6, 22, 1));
}

/// Returns the host CPU as the host OS reports it, without its brand string: the first
/// processor of `/proc/cpuinfo` on Linux, and `PROCESSOR_IDENTIFIER` on Windows, such as
/// `Intel64 Family 6 Model 85 Stepping 4, GenuineIntel`.
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
fn reported_cpu() -> Option<HostCpu> {
    if cfg!(windows) {
        let identifier = std::env::var("PROCESSOR_IDENTIFIER").ok()?;
        let (description, vendor) = identifier.split_once(", ")?;
        let words: Vec<&str> = description.split_whitespace().collect();
        let number = |name: &str| {
            let position = words.iter().position(|word| *word == name)?;
            words.get(position + 1)?.parse().ok()
        };
        return Some(cpu(
            vendor.trim(),
            number("Family")?,
            number("Model")?,
            number("Stepping")?,
            "",
        ));
    }
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let field = |name: &str| {
        cpuinfo.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_owned())
        })
    };
    Some(cpu(
        &field("vendor_id")?,
        field("cpu family")?.parse().ok()?,
        field("model")?.parse().ok()?,
        field("stepping")?.parse().ok()?,
        "",
    ))
}

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
#[test]
fn the_host_cpu_is_the_one_that_the_host_os_reports() {
    let current = HostCpu::current().unwrap();
    let reported = reported_cpu().expect("the host OS reports the CPU");
    assert_eq!(
        HostCpu {
            brand: String::new(),
            ..current.clone()
        },
        reported
    );
    if cfg!(target_os = "linux") && !current.brand.is_empty() {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap();
        assert!(
            cpuinfo
                .lines()
                .any(|line| line.split_once(':').is_some_and(|(key, value)| {
                    key.trim() == "model name" && value.trim() == current.brand
                })),
            "{current:?}"
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[test]
fn hosts_without_cpuid_identify_no_cpu() {
    assert_eq!(HostCpu::current(), None);
}

#[test]
fn request_links_prefill_the_cpu() {
    let granite_rapids = cpu("GenuineIntel", 6, 173, 1, "Intel(R) Xeon(R) 6973P-C");
    assert_eq!(granite_rapids.signature(), "GenuineIntel 6/173/1");
    assert_eq!(
        granite_rapids.describe(),
        "GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C)"
    );
    // The link that OpenVMM's fallback warning gives the same CPU.
    assert_eq!(
        granite_rapids.request_url(),
        "https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml\
             &title=CPU%20profile%3A%20GenuineIntel%206%2F173%2F1%20%28Intel%28R%29\
             %20Xeon%28R%29%206973P-C%29&signature=GenuineIntel%206%2F173%2F1\
             &cpu=Intel%28R%29%20Xeon%28R%29%206973P-C"
    );
    // A CPU without a brand string prefills only its signature.
    let brandless = cpu("GenuineIntel", 6, 173, 1, "");
    assert_eq!(brandless.describe(), "GenuineIntel 6/173/1");
    assert_eq!(
        brandless.request_url(),
        format!(
            "{REQUEST_URL}&title=CPU%20profile%3A%20GenuineIntel%206%2F173%2F1\
                 &signature=GenuineIntel%206%2F173%2F1"
        )
    );
    assert!(
        cpu("GenuineIntel", 6, 173, 1, "&=?#%+ é")
            .request_url()
            .ends_with("&cpu=%26%3D%3F%23%25%2B%20%C3%A9")
    );
}

#[test]
fn unserved_intel_and_amd_cpus_get_the_fallback_and_a_profile_request() {
    let tiger_lake = cpu(
        "GenuineIntel",
        6,
        140,
        1,
        "11th Gen Intel(R) Core(TM) i7-1185G7",
    );
    let guidance = guide(None, &tiger_lake).unwrap();
    let lines: Vec<&str> = guidance.lines().collect();
    assert!(
        lines.iter().all(|line| line.starts_with("nvx: ")),
        "{guidance}"
    );
    // auto falls back on the CPU, so the guidance says what auto does there, not which code
    // ended this run, which nvx-rs run cannot read.
    assert_eq!(
        lines[0],
        "nvx: no built-in CPU profile serves this host's CPU, GenuineIntel 6/140/1 (11th Gen \
             Intel(R) Core(TM) i7-1185G7), so --cpu-profile auto, the default, falls back on it to \
             a CPU profile derived from this host, which OpenVMM warns about with \
             NVX-CPU-PROFILE-FALLBACK:; the built-in profiles cover skylake-sp 6/85 steppings \
             0-4, icelake-sp 6/106, emeraldrapids 6/207, alderlake 6/151 and 6/154, milan 25/1, \
             genoa 25/17, turin 26/2."
    );
    assert!(lines[1].contains("OpenVMM's error above names the cause"));
    assert!(!guidance.contains(PROFILE_HOST_UNKNOWN));
    // It asks for the CPU's fingerprint through the CPU profile request form, which the link
    // prefills with the CPU.
    let request = format!("nvx:   {}", tiger_lake.request_url());
    assert_eq!(
        lines[2..],
        [
            "nvx: Help NVX add a built-in profile for this CPU: run",
            "nvx:   /nvx/openvmm --hypervisor kvm --cpu-fingerprint fingerprint.json",
            "nvx: and attach fingerprint.json to a CPU profile request:",
            request.as_str(),
            "nvx: https://github.com/microsoft/nvx/issues/408 tracks CPU profiles for more \
                 Intel CPUs.",
        ]
    );
    assert_eq!(guide(Some("auto"), &tiger_lake).as_ref(), Some(&guidance));
    // A host profile that fails on this CPU fails for a reason that OpenVMM's own error
    // explains.
    assert_eq!(guide(Some(HOST_PROFILE), &tiger_lake), None);

    // Host profiles serve AMD CPUs too: an AMD CPU without a built-in profile, such as
    // Raphael (Ryzen 7000), falls back as an Intel one does, with the issue that tracks
    // profiles for more AMD CPUs.
    let raphael = cpu("AuthenticAMD", 25, 97, 2, "");
    let guidance = guide(None, &raphael).unwrap();
    assert!(guidance.contains(
        "this host's CPU, AuthenticAMD 25/97/2, so --cpu-profile auto, the default, falls \
             back on it"
    ));
    assert!(guidance.contains(&raphael.request_url()));
    assert!(guidance.ends_with(
        "\nnvx: https://github.com/microsoft/nvx/issues/409 tracks CPU profiles for more AMD \
             CPUs."
    ));
    assert!(!guidance.contains("issues/408"));
    assert_eq!(guide(Some(HOST_PROFILE), &raphael), None);
}

#[test]
fn unserved_cpus_of_other_vendors_get_no_profile_request() {
    // Another vendor's CPU fails with either profile, and gets no request form and no issue:
    // the time ABI does not serve its CPUs.
    let hygon = cpu("HygonGenuine", 24, 0, 1, "");
    let guidance = guide(Some("auto"), &hygon).unwrap();
    assert_eq!(
        guidance,
        "nvx: no built-in CPU profile serves this host's CPU, HygonGenuine 24/0/1, so a cold \
             boot with --cpu-profile auto, the default, fails on it with E_PROFILE_HOST_UNKNOWN; \
             the built-in profiles cover skylake-sp 6/85 steppings 0-4, icelake-sp 6/106, \
             emeraldrapids 6/207, alderlake 6/151 and 6/154, milan 25/1, genoa 25/17, turin \
             26/2.\nnvx: --cpu-profile host cannot boot on it either: host CPU profiles serve only \
             Intel and AMD CPUs."
    );
    assert_eq!(guide(None, &hygon).as_ref(), Some(&guidance));
    assert_eq!(guide(Some(HOST_PROFILE), &hygon).as_ref(), Some(&guidance));
}

#[test]
fn served_cpus_explicit_profiles_and_unknown_cpus_get_none() {
    let tiger_lake = cpu("GenuineIntel", 6, 140, 1, "");
    let raphael = cpu("AuthenticAMD", 25, 97, 2, "");
    let hygon = cpu("HygonGenuine", 24, 0, 1, "");
    let alder_lake = cpu("GenuineIntel", 6, 154, 3, "");
    let milan = cpu("AuthenticAMD", 25, 1, 1, "");
    let genoa = cpu("AuthenticAMD", 25, 17, 1, "");
    let turin = cpu("AuthenticAMD", 26, 2, 1, "");
    for (cpu_profile, host) in [
        (None, &alder_lake),
        (Some(HOST_PROFILE), &alder_lake),
        (None, &milan),
        (Some(HOST_PROFILE), &milan),
        (None, &genoa),
        (None, &turin),
        (Some(HOST_PROFILE), &turin),
        (Some("intel.alderlake.v1"), &tiger_lake),
        (Some("intel.alderlake.v1"), &raphael),
        (Some("intel.skylake-sp.v1"), &milan),
        (Some("intel.skylake-sp.v1"), &hygon),
    ] {
        assert_eq!(guide(cpu_profile, host), None, "{cpu_profile:?} {host:?}");
    }
    assert_eq!(guidance(None, None, "/nvx/openvmm", "kvm"), None);
}
