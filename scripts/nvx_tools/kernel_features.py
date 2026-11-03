"""The feature inventory of the microVM kernel's configuration.

`kernel/config-microvm` is generated: `make allnoconfig` with this inventory's
settings as its fragment, so that every symbol that a user could set is off
unless a feature below sets it. Every other symbol that the configuration sets,
Kconfig sets because of these settings: a symbol that they select, a symbol
without a prompt that the architecture, the toolchain, or other symbols decide,
or a symbol whose prompt they hide and whose default holds.
`kernel/config-microvm.derived` records each one of those with the rule that
sets it, and `nvx.py check-kernel-config` and the kernel build check both files
against this inventory (doc/build.md#kernel-configuration).

Each feature says why NVX needs it, which NVX components consume it, and which
checks or scenarios exercise it. Its `settings` are the assignments that it
makes; its `expects` are assignments that Kconfig derives, or that another
feature makes, on which it relies, and which the checks require too.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class KernelFeature:
    """A feature of the microVM kernel that NVX enables or keeps off on purpose."""

    name: str
    summary: str
    rationale: str
    consumers: tuple[str, ...]
    validation: tuple[str, ...]
    settings: tuple[str, ...]
    expects: tuple[str, ...] = ()


KERNEL_FEATURES: tuple[KernelFeature, ...] = (
    KernelFeature(
        name="kernel-build",
        summary="64-bit build policy that every other setting rests on",
        rationale=(
            "The kernel build runs make without ARCH, so ARCH is x86 and "
            "64BIT is a prompt that the inventory sets. WERROR keeps the "
            "pinned toolchain's build free of warnings. EXPERT makes the "
            "options that Linux otherwise forces on, such as the virtual "
            "terminal, the PC speaker, and obsolete system calls, settings "
            "like any other, so that each is an inventory decision; it "
            "selects DEBUG_KERNEL, which only shows the debug options, all "
            "of which stay off unless a feature sets them. GCC plugins stay "
            "off on toolchains that offer them, such as Azure Linux 3's GCC, "
            "so that every toolchain builds the same configuration. Section "
            "mismatches stay warnings, as upstream builds them."
        ),
        consumers=("nvx.py build-kernel", "the Docker kernel build"),
        validation=(
            "build-kernel's kernel configuration checks",
            "CI's guest-artifact builds",
        ),
        settings=(
            "CONFIG_64BIT=y",
            "CONFIG_WERROR=y",
            "CONFIG_EXPERT=y",
            "# CONFIG_GCC_PLUGINS is not set",
            "CONFIG_SECTION_MISMATCH_WARN_ONLY=y",
        ),
        expects=("CONFIG_X86_64=y",),
    ),
    KernelFeature(
        name="direct-boot",
        summary="ACPI-free MP-table boot with virtio-mmio devices from the command line",
        rationale=(
            "OpenVMM's Linux direct loader builds an Intel MP table and "
            "boot_params, and no ACPI tables, PVH notes, or PCI bus; the "
            "machine profile names every virtio-mmio device on the kernel "
            "command line (doc/design/5.1-boot-protocol.md)."
        ),
        consumers=("OpenVMM's Linux direct loader", "the machine profile"),
        validation=(
            "every microVM scenario's boot",
            "the interrupt-map scenario",
        ),
        settings=(
            "# CONFIG_ACPI is not set",
            "# CONFIG_PVH is not set",
            "# CONFIG_PCI is not set",
            "CONFIG_VIRTIO_MENU=y",
            "CONFIG_VIRTIO_MMIO=y",
            "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
        ),
        expects=(
            "CONFIG_X86_MPPARSE=y",
            "CONFIG_X86_LOCAL_APIC=y",
            "CONFIG_X86_IO_APIC=y",
            "CONFIG_VIRTIO=y",
        ),
    ),
    KernelFeature(
        name="processors",
        summary="SMP on the Intel and AMD processors of the CPU profiles",
        rationale=(
            "A microVM has 1, 2, 4, or 8 virtual processors, whose CPU "
            "profiles serve Intel and AMD hosts alone, so the other vendors' "
            "support stays off. Clone repair and the snapshot helper bring "
            "processors online after a restore. The local APIC driver keeps "
            "its x2APIC support, and PAT, on MTRRs, gives mappings their "
            "memory types, as on every x86 kernel."
        ),
        consumers=(
            "every multiprocessor microVM",
            "nvx::guest::clone::repair",
            "guest/common/nvx-snapshot",
        ),
        validation=(
            "the --processors runs of the microVM scenarios",
            "the runtime adapter's clone tests",
        ),
        settings=(
            "CONFIG_SMP=y",
            "CONFIG_X86_X2APIC=y",
            "CONFIG_PROCESSOR_SELECT=y",
            "CONFIG_CPU_SUP_INTEL=y",
            "CONFIG_CPU_SUP_AMD=y",
            "CONFIG_MTRR=y",
            "CONFIG_X86_PAT=y",
        ),
        expects=("CONFIG_HOTPLUG_CPU=y",),
    ),
    KernelFeature(
        name="scheduling",
        summary="Voluntary preemption, cache-aware scheduling domains, and static branches",
        rationale=(
            "The kernel builds full preemption and runs voluntary preemption "
            "(PREEMPT_DYNAMIC), with preemptible RCU, whose grace periods the "
            "time ABI's stall suppression follows. The multi-core and cluster "
            "scheduling domains give the virtual processors that share a "
            "cache a domain, without which a wakeup would not look for an "
            "idle processor among them. Static branches and the scheduler's "
            "omitted frame pointers keep the hot paths as fast as upstream's "
            "defaults make them."
        ),
        consumers=("every microVM", "the time ABI's stall suppression"),
        validation=(
            "the time ABI checks",
            "the --processors runs of the microVM scenarios",
            "the benchmarks",
        ),
        settings=(
            "CONFIG_PREEMPT_VOLUNTARY=y",
            "CONFIG_PREEMPT_DYNAMIC=y",
            "CONFIG_SCHED_MC=y",
            "CONFIG_SCHED_CLUSTER=y",
            "CONFIG_JUMP_LABEL=y",
            "CONFIG_SCHED_OMIT_FRAME_POINTER=y",
        ),
        expects=("CONFIG_PREEMPT_RCU=y", "CONFIG_SCHED_SMT=y"),
    ),
    KernelFeature(
        name="time-abi",
        summary="The clock, tick, and stall settings of the time ABI",
        rationale=(
            "The guest identifies its hypervisor only through the time ABI's "
            "identity, so the paravirtualized-guest core stays and the "
            "Hyper-V and KVM guest code, kvmclock, haltpoll, and cpufreq, "
            "whose intel_pstate probes MSRs that the microVM lacks, stay off. "
            "Ticks are tickless-idle with high-resolution timers at a low "
            "tick rate (doc/design/3.2-clock-and-time.md). Conformance check "
            "C8 requires the RCU stall timeouts, and the violation watcher "
            "follows the kernel log from boot, which a 256 KiB buffer keeps "
            "whole. nvx-time reads the frequency and identity MSRs through "
            "/dev/cpu/N/msr and the time-sample port through ioperm."
        ),
        consumers=("nvx-time", "nvx-time-probe", "the time ABI"),
        validation=(
            "nvx-time's boot, capture, and restore checks in every scenario",
            "the time-abi scenarios",
            "scripts/test_time_abi.py",
        ),
        settings=(
            "CONFIG_HYPERVISOR_GUEST=y",
            "CONFIG_PARAVIRT=y",
            "# CONFIG_HYPERV is not set",
            "# CONFIG_KVM_GUEST is not set",
            "# CONFIG_CPU_FREQ is not set",
            "CONFIG_HZ_100=y",
            "CONFIG_NO_HZ_IDLE=y",
            "CONFIG_HIGH_RES_TIMERS=y",
            "CONFIG_RCU_CPU_STALL_TIMEOUT=21",
            "CONFIG_RCU_EXP_CPU_STALL_TIMEOUT=0",
            "CONFIG_LOG_BUF_SHIFT=18",
            "CONFIG_PRINTK=y",
            "CONFIG_PRINTK_TIME=y",
        ),
        expects=(
            "CONFIG_X86_TSC=y",
            "CONFIG_X86_MSR=y",
            "CONFIG_X86_IOPL_IOPERM=y",
        ),
    ),
    KernelFeature(
        name="production-watchdogs",
        summary="The soft-lockup and hung-task detectors off outside the CI debug kernel",
        rationale=(
            "Restores make kernel time jump, so production kernels build no "
            "soft-lockup or hung-task detector; the CI debug kernel's "
            "fragment, kernel/config-microvm-debug, turns them on to prove "
            "that restores never trip them."
        ),
        consumers=("every production kernel", "the CI debug kernel"),
        validation=(
            "build-kernel's watchdog check",
            "the debug-kernel microVM jobs",
        ),
        settings=(
            "# CONFIG_SOFTLOCKUP_DETECTOR is not set",
            "# CONFIG_DETECT_HUNG_TASK is not set",
        ),
    ),
    KernelFeature(
        name="byte-channel-console",
        summary="The xe9 console on the byte channel, hvc0",
        rationale=(
            "The machine profile's base command line selects the xe9 console "
            "of kernel patch 0002, console=hvc0, and the xe9 early console of "
            "patch 0001, which early_printk.c holds. The early console also "
            "needs SERIAL_EARLYCON, which no symbol of this configuration "
            "selects, so earlycon=xe9 has no effect and hvc0 prints the boot "
            "log once its console initcall runs."
        ),
        consumers=("the machine profile's console=hvc0", "guest/common/init"),
        validation=("every microVM scenario's console markers",),
        settings=(
            "CONFIG_TTY=y",
            "CONFIG_HVC_XE9=y",
            "CONFIG_EARLY_PRINTK=y",
        ),
        expects=("CONFIG_HVC_DRIVER=y",),
    ),
    KernelFeature(
        name="virtio-console",
        summary="Virtio consoles, the boot console hvc1 and the control console hvc2",
        rationale=(
            "The boot virtio-console becomes the kernel console, and the "
            "managed lifecycle's control session runs on hvc2 "
            "(doc/design/3.3-consoles.md)."
        ),
        consumers=("nvx-managed-agent", "nvx-init-agent", "nvx-rs sandbox"),
        validation=("the managed and sandbox scenarios",),
        settings=("CONFIG_VIRTIO_CONSOLE=y",),
        expects=(
            "CONFIG_HVC_DRIVER=y",
            "CONFIG_VIRTIO=y",
            "CONFIG_VIRTIO_MMIO=y",
            "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
        ),
    ),
    KernelFeature(
        name="shared-interrupt-status",
        summary="The shared virtio interrupt-status page of kernel patch 0003",
        rationale=(
            "virtio-mmio reads its devices' interrupt status from the shared "
            "page (doc/design/2.4-interrupt-and-io-architecture.md)."
        ),
        consumers=("virtio_mmio",),
        validation=("the virtio-features and interrupt-map scenarios",),
        settings=("CONFIG_VIRTIO_MMIO_SHARED_STATUS=y",),
    ),
    KernelFeature(
        name="platform-control",
        summary="/dev/mem, /dev/port, ioperm, and /dev/cpu/N/msr for the guest programs",
        rationale=(
            "The guest programs reach the platform-control device's ports "
            "through ioperm or /dev/port, the virtio-mmio windows through "
            "/dev/mem, and the time ABI's MSRs through /dev/cpu/N/msr. Strict "
            "/dev/mem keeps RAM out of reach; IO_STRICT_DEVMEM would refuse "
            "the device windows that drivers claim, so it stays off."
        ),
        consumers=(
            "nvx-exit",
            "nvx-mmio-write",
            "nvx-virtio-restore-probe",
            "nvx-time",
            "nvx::guest::sys::port",
        ),
        validation=(
            "every microVM scenario's exit status",
            "the virtio restore scenarios",
            "the snapshot scenarios",
        ),
        settings=(
            "CONFIG_DEVMEM=y",
            "CONFIG_DEVPORT=y",
            "CONFIG_STRICT_DEVMEM=y",
            "# CONFIG_IO_STRICT_DEVMEM is not set",
            "CONFIG_X86_IOPL_IOPERM=y",
            "CONFIG_X86_MSR=y",
        ),
    ),
    KernelFeature(
        name="guest-init",
        summary="Initramfs boot and the pseudo-filesystems that /init mounts",
        rationale=(
            "Every guest boots a gzip initramfs, whose files keep their "
            "normalized timestamps, and /init mounts procfs, sysfs, devtmpfs, "
            "and tmpfs, and runs ELF programs and #! scripts."
        ),
        consumers=("guest/common/init", "every guest image"),
        validation=("every microVM scenario's boot",),
        settings=(
            "CONFIG_BLK_DEV_INITRD=y",
            "CONFIG_RD_GZIP=y",
            "CONFIG_INITRAMFS_PRESERVE_MTIME=y",
            "CONFIG_DEVTMPFS=y",
            "CONFIG_PROC_FS=y",
            "CONFIG_PROC_SYSCTL=y",
            "CONFIG_SYSFS=y",
            "CONFIG_TMPFS=y",
            "CONFIG_BINFMT_ELF=y",
            "CONFIG_BINFMT_SCRIPT=y",
        ),
    ),
    KernelFeature(
        name="memory-hotplug",
        summary="Memory blocks that clone repair adds through the probe interface",
        rationale=(
            "After a restore, clone repair and the snapshot helper add the "
            "memory blocks of the restored RAM through "
            "/sys/devices/system/memory/probe and online them themselves."
        ),
        consumers=("nvx::guest::clone::repair", "guest/common/nvx-snapshot"),
        validation=(
            "the runtime adapter's clone tests",
            "the snapshot scenarios with a RAM target",
        ),
        settings=(
            "CONFIG_MEMORY_HOTPLUG=y",
            "CONFIG_ARCH_MEMORY_PROBE=y",
            "CONFIG_MHP_DEFAULT_ONLINE_TYPE_OFFLINE=y",
        ),
        expects=("CONFIG_SPARSEMEM_VMEMMAP=y",),
    ),
    KernelFeature(
        name="memory-management",
        summary="Compaction, page-table reclaim, and slab defaults",
        rationale=(
            "Long-running sandboxes keep memory compact and reclaim empty "
            "page tables, and slabs merge and keep per-CPU partial lists, "
            "as upstream defaults them."
        ),
        consumers=("every microVM",),
        validation=("the benchmarks' memory measurements",),
        settings=(
            "CONFIG_COMPACTION=y",
            "CONFIG_PT_RECLAIM=y",
            "CONFIG_SLAB_MERGE_DEFAULT=y",
            "CONFIG_SLUB_CPU_PARTIAL=y",
        ),
    ),
    KernelFeature(
        name="block-storage",
        summary="virtio-blk disks and their GPT partitions",
        rationale=(
            "Sandbox layers and scratch are virtio-blk disks, and a layer may "
            "be a GPT disk whose partitions the kernel creates "
            "(nvx::guest::gpt); no NVX image has an MBR partition table."
        ),
        consumers=(
            "nvx-init-agent",
            "nvx::guest::root",
            "nvx::guest::clone",
        ),
        validation=("the sandbox scenarios", "the runtime adapter's GPT tests"),
        settings=(
            "CONFIG_BLOCK=y",
            "CONFIG_BLK_DEV=y",
            "CONFIG_VIRTIO_BLK=y",
            "CONFIG_PARTITION_ADVANCED=y",
            "CONFIG_EFI_PARTITION=y",
        ),
    ),
    KernelFeature(
        name="sandbox-filesystems",
        summary="EROFS layers under overlayfs, on an ext4 or tmpfs upper",
        rationale=(
            "The layered sandbox root mounts EROFS layers, compressed with "
            "LZ4 or Zstandard, under overlayfs, whose upper is the ext4 "
            "scratch or a tmpfs (doc/design/4.1-guest-filesystem-and-image-model.md). "
            "Every layer keeps extended attributes, POSIX ACLs, and security "
            "labels, such as file capabilities, through copy-up. Overlayfs "
            "never follows a redirect that it did not create, and the scratch "
            "template's metadata checksums use the optimized CRCs."
        ),
        consumers=("nvx-init-agent", "nvx::guest::root", "generic sandbox workloads"),
        validation=("the sandbox scenarios", "the RAM-upper scenarios"),
        settings=(
            "CONFIG_EXT4_FS=y",
            "CONFIG_EXT4_FS_POSIX_ACL=y",
            "CONFIG_EXT4_FS_SECURITY=y",
            "CONFIG_CRC_OPTIMIZATIONS=y",
            "CONFIG_MISC_FILESYSTEMS=y",
            "CONFIG_EROFS_FS=y",
            "CONFIG_EROFS_FS_XATTR=y",
            "CONFIG_EROFS_FS_POSIX_ACL=y",
            "CONFIG_EROFS_FS_SECURITY=y",
            "CONFIG_EROFS_FS_ZIP=y",
            "CONFIG_EROFS_FS_ZIP_ZSTD=y",
            "CONFIG_OVERLAY_FS=y",
            "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
            "CONFIG_TMPFS_POSIX_ACL=y",
            "CONFIG_TMPFS_XATTR=y",
        ),
    ),
    KernelFeature(
        name="sandbox-isolation",
        summary="The namespaces, cgroups, seccomp, and BPF of the sandbox",
        rationale=(
            "The workload gets private mount, PID, and UTS namespaces, and "
            "sibling memory and process-count cgroups whose freezing captures "
            "it; cgroup writeback charges its dirty scratch pages to its "
            "memory cgroup (doc/design/4.3-isolation-and-resource-model.md). "
            "Generic sandbox workloads filter system calls with seccomp and "
            "control devices with cgroup BPF programs, and unprivileged BPF "
            "stays off. Without capabilities or user namespaces, workloads "
            "create no namespaces of their own, so the other namespace and "
            "cgroup controller types stay off."
        ),
        consumers=(
            "nvx-init-agent",
            "nvx-container-launch",
            "nvx::guest::cgroup",
            "nvx::guest::entry",
            "guest/common/nvx-snapshot",
            "generic sandbox workloads",
        ),
        validation=("the sandbox and managed scenarios", "the runtime adapter"),
        settings=(
            "CONFIG_NAMESPACES=y",
            "CONFIG_UTS_NS=y",
            "CONFIG_PID_NS=y",
            "# CONFIG_USER_NS is not set",
            "CONFIG_CGROUPS=y",
            "CONFIG_MEMCG=y",
            "CONFIG_CGROUP_PIDS=y",
            "CONFIG_BLK_CGROUP=y",
            "CONFIG_CGROUP_BPF=y",
            "CONFIG_BPF_SYSCALL=y",
            "CONFIG_BPF_UNPRIV_DEFAULT_OFF=y",
            "CONFIG_SECCOMP=y",
        ),
        expects=("CONFIG_SECCOMP_FILTER=y", "CONFIG_CGROUP_WRITEBACK=y"),
    ),
    KernelFeature(
        name="guest-internal-networking",
        summary="Network namespaces and veth pairs for guest services (#309)",
        rationale=(
            "Early guest services create private network links between "
            "namespaces without a module loader, as #309 added."
        ),
        consumers=("guest services of product images",),
        validation=("build-kernel's sandbox group",),
        settings=("CONFIG_NET_NS=y", "CONFIG_VETH=y"),
    ),
    KernelFeature(
        name="unix-sockets",
        summary="Unix-domain sockets",
        rationale=(
            "Sandbox workloads and the guest images' userland talk over "
            "Unix-domain sockets, with their out-of-band data."
        ),
        consumers=("generic sandbox workloads", "every guest image"),
        validation=("build-kernel's sandbox group",),
        settings=("CONFIG_UNIX=y", "CONFIG_AF_UNIX_OOB=y"),
    ),
    KernelFeature(
        name="network",
        summary="IPv4 and IPv6 over virtio-net",
        rationale=(
            "The portable network profile configures a dual-stack virtio-net "
            "NIC (doc/design/3.5-network.md); SIT would add an sit0 device "
            "that decapsulates IPv6 from IPv4. nvx-ingress finds the "
            "workload's listeners through the TCP socket diagnostics. Receive "
            "steering, byte queue limits, and per-CPU device references keep "
            "the data path as upstream defaults it."
        ),
        consumers=("guest/common/init", "nvx-ingress", "nvx-rs run --net"),
        validation=("the network scenarios", "the netsnap benchmarks"),
        settings=(
            "CONFIG_NET=y",
            "CONFIG_INET=y",
            "CONFIG_INET_DIAG=y",
            "CONFIG_IPV6=y",
            "# CONFIG_IPV6_SIT is not set",
            "CONFIG_NETDEVICES=y",
            "CONFIG_NET_CORE=y",
            "CONFIG_VIRTIO_NET=y",
            "CONFIG_RPS=y",
            "CONFIG_NET_FLOW_LIMIT=y",
            "CONFIG_BQL=y",
            "CONFIG_PCPU_DEV_REFCNT=y",
        ),
        expects=("CONFIG_INET_TCP_DIAG=y",),
    ),
    KernelFeature(
        name="host-filesystem",
        summary="virtio-fs host shares",
        rationale=(
            "Live host shares and a managed sandbox's host export are "
            "virtio-fs mounts (doc/design/3.6-host-filesystem.md)."
        ),
        consumers=("nvx-hostmount", "nvx-init-agent"),
        validation=("the virtio-fs scenarios",),
        settings=("CONFIG_FUSE_FS=y", "CONFIG_VIRTIO_FS=y"),
    ),
    KernelFeature(
        name="share-writeback",
        summary="debugfs, whose writeback statistics drain the host shares before a capture",
        rationale=(
            "Before a capture, the snapshot helper writes back the pages that "
            "shared mappings dirtied on the host shares until the writeback "
            "lists of their backing devices are empty, and only debugfs's "
            "bdi/MAJ:MIN/stats counts the inodes on those lists; the helper "
            "mounts debugfs in a private mount namespace for each count "
            "(guest preparation step 4 of "
            "doc/design/5.3-snapshot-protocol.md). The block layer's own "
            "debugfs files stay off."
        ),
        consumers=("guest/common/nvx-snapshot",),
        validation=("the snapshot scenarios with host shares",),
        settings=("CONFIG_DEBUG_FS=y",),
        expects=("CONFIG_DEBUG_FS_ALLOW_ALL=y",),
    ),
    KernelFeature(
        name="cpu-mitigations",
        summary="Every CPU vulnerability mitigation that upstream defaults on",
        rationale=(
            "The workload is untrusted relative to the guest agents and the "
            "guest kernel, which keep upstream's CPU vulnerability "
            "mitigations."
        ),
        consumers=("every microVM",),
        validation=("the benchmarks",),
        settings=(
            "CONFIG_CPU_MITIGATIONS=y",
            "CONFIG_MITIGATION_PAGE_TABLE_ISOLATION=y",
            "CONFIG_MITIGATION_RETPOLINE=y",
            "CONFIG_MITIGATION_RETHUNK=y",
            "CONFIG_MITIGATION_UNRET_ENTRY=y",
            "CONFIG_MITIGATION_CALL_DEPTH_TRACKING=y",
            "CONFIG_MITIGATION_IBPB_ENTRY=y",
            "CONFIG_MITIGATION_IBRS_ENTRY=y",
            "CONFIG_MITIGATION_SRSO=y",
            "CONFIG_MITIGATION_GDS=y",
            "CONFIG_MITIGATION_RFDS=y",
            "CONFIG_MITIGATION_SPECTRE_BHI=y",
            "CONFIG_MITIGATION_MDS=y",
            "CONFIG_MITIGATION_TAA=y",
            "CONFIG_MITIGATION_MMIO_STALE_DATA=y",
            "CONFIG_MITIGATION_L1TF=y",
            "CONFIG_MITIGATION_RETBLEED=y",
            "CONFIG_MITIGATION_SPECTRE_V1=y",
            "CONFIG_MITIGATION_SPECTRE_V2=y",
            "CONFIG_MITIGATION_SRBDS=y",
            "CONFIG_MITIGATION_SSB=y",
            "CONFIG_MITIGATION_ITS=y",
            "CONFIG_MITIGATION_TSA=y",
        ),
    ),
    KernelFeature(
        name="hardening",
        summary="Read-only runtime code, stack protection, and a W+X audit",
        rationale=(
            "x86 makes runtime-generated code, such as the ITS mitigation "
            "thunks, read-only only through STRICT_MODULE_RWX "
            "(ARCH_HAS_EXECMEM_ROX), which needs MODULES. No module is ever "
            "built or shipped: there is no autoload helper, no symbol is "
            "exported, and init disables module loading before anything else "
            "runs. Stacks are protected, guarded, and zeroed, indirect "
            "branches are tracked, user mode cannot read descriptor tables, "
            "and the boot-time W+X audit, which nvx-time's boot check reads, "
            "covers the kernel's page tables. The kernel can randomize each "
            "system call's stack offset, but, as it always has, only when its "
            "command line sets randomize_kstack_offset=on."
        ),
        consumers=("guest/common/init", "nvx-time", "every microVM"),
        validation=(
            "build-kernel's hardening group",
            "nvx-time's W+X boot check",
        ),
        settings=(
            "CONFIG_MODULES=y",
            'CONFIG_MODPROBE_PATH=""',
            "CONFIG_TRIM_UNUSED_KSYMS=y",
            "CONFIG_DEBUG_WX=y",
            "CONFIG_STACKPROTECTOR=y",
            "CONFIG_STACKPROTECTOR_STRONG=y",
            "CONFIG_VMAP_STACK=y",
            "CONFIG_RANDOMIZE_KSTACK_OFFSET=y",
            "# CONFIG_RANDOMIZE_KSTACK_OFFSET_DEFAULT is not set",
            "CONFIG_INIT_STACK_ALL_ZERO=y",
            "CONFIG_X86_KERNEL_IBT=y",
            "CONFIG_X86_UMIP=y",
        ),
        expects=(
            "CONFIG_STRICT_KERNEL_RWX=y",
            "CONFIG_STRICT_MODULE_RWX=y",
            "CONFIG_ARCH_HAS_EXECMEM_ROX=y",
            "CONFIG_MITIGATION_ITS=y",
        ),
    ),
    KernelFeature(
        name="linux-abi",
        summary="The system calls and interfaces that workloads expect of Linux",
        rationale=(
            "Workloads are arbitrary programs of the Alpine, Ubuntu, and "
            "Azure Linux userlands and of container images, 32-bit ones "
            "included, and the guest agents' Rust and musl runtimes use "
            "futexes, epoll, and eventfd, so the kernel keeps the system "
            "calls and filesystem interfaces that Linux offers unless EXPERT "
            "removes them; only obsolete ones, such as the 16-bit user-ID "
            "and sgetmask calls and 16-bit segments, stay off."
        ),
        consumers=("generic sandbox workloads", "the guest agents"),
        validation=("every microVM scenario", "the runtime adapter"),
        settings=(
            "CONFIG_MULTIUSER=y",
            "CONFIG_SYSVIPC=y",
            "CONFIG_POSIX_TIMERS=y",
            "CONFIG_FUTEX=y",
            "CONFIG_EPOLL=y",
            "CONFIG_SIGNALFD=y",
            "CONFIG_TIMERFD=y",
            "CONFIG_EVENTFD=y",
            "CONFIG_SHMEM=y",
            "CONFIG_AIO=y",
            "CONFIG_IO_URING=y",
            "CONFIG_ADVISE_SYSCALLS=y",
            "CONFIG_MEMBARRIER=y",
            "CONFIG_RSEQ=y",
            "CONFIG_CACHESTAT_SYSCALL=y",
            "CONFIG_FHANDLE=y",
            "CONFIG_CROSS_MEMORY_ATTACH=y",
            "CONFIG_SECRETMEM=y",
            "CONFIG_FILE_LOCKING=y",
            "CONFIG_DNOTIFY=y",
            "CONFIG_INOTIFY_USER=y",
            "CONFIG_PROC_PAGE_MONITOR=y",
            "CONFIG_UNIX98_PTYS=y",
            "CONFIG_ELF_CORE=y",
            "CONFIG_COREDUMP=y",
            "CONFIG_CORE_DUMP_DEFAULT_ELF_HEADERS=y",
            "CONFIG_IA32_EMULATION=y",
            "CONFIG_COMPAT_32BIT_TIME=y",
            "CONFIG_X86_VSYSCALL_EMULATION=y",
            "CONFIG_MODIFY_LDT_SYSCALL=y",
            "CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS=y",
        ),
    ),
    KernelFeature(
        name="diagnostics",
        summary="Readable oopses and the sysrq trigger of the entry contract",
        rationale=(
            "Oopses and BUGs carry symbol and error names, which CI logs "
            "need. The layered sandbox entry makes /proc/sysrq-trigger "
            "read-only for user ID 0, which the runtime adapter's "
            "vm_layered test checks."
        ),
        consumers=("CI logs", "nvx::guest::sys::entry"),
        validation=("the runtime adapter's vm_layered test",),
        settings=(
            "CONFIG_BUG=y",
            "CONFIG_DEBUG_BUGVERBOSE=y",
            "CONFIG_KALLSYMS=y",
            "CONFIG_SYMBOLIC_ERRNAME=y",
            "CONFIG_MAGIC_SYSRQ=y",
        ),
    ),
)

# The kernel build's checks (scripts/nvx_tools/build.py) and the source and
# release checks (scripts/nvx_tools/release.py), each the features whose
# settings and expectations it requires.
KERNEL_CONFIG_CHECKS: dict[str, tuple[str, ...]] = {
    "direct-boot": ("direct-boot",),
    "virtio-console": ("virtio-console",),
    "shared-status": ("shared-interrupt-status",),
    "sandbox": (
        "block-storage",
        "sandbox-filesystems",
        "sandbox-isolation",
        "guest-internal-networking",
        "unix-sockets",
    ),
    "network": ("network",),
    "time-abi": ("time-abi",),
    "hardening": ("hardening",),
}
