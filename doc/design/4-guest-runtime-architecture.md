# Guest Runtime Architecture

[Architecture documentation](README.md)

The NVX guest runtime turns one microVM into one workload sandbox. The guest
agents supervise the workload for the life of the VM. In the
[layered sandbox root](4.1-guest-filesystem-and-image-model.md#411-layered-sandbox-root),
they remain outside the workload's namespaces; the
[blockless managed root](4.1-guest-filesystem-and-image-model.md#413-blockless-managed-root)
creates none and contains the workload with its credentials and a cgroup. The
architecture does not introduce pod infrastructure, a pause container, or a
guest-side service for creating arbitrary additional containers.

This chapter describes how the guest turns the virtual machine into an
isolated workload sandbox.

## Table of Contents

- [4.1 Guest Filesystem and Image Model](4.1-guest-filesystem-and-image-model.md):
  Explains image layers, the writable upper, overlay assembly, and live shares.
- [4.2 Guest Agents and Workload Model](4.2-guest-agents-and-workload-model.md):
  Defines the relationship between the guest agents and the workload.
- [4.3 Isolation and Resource Model](4.3-isolation-and-resource-model.md):
  Describes identities, namespaces, capabilities, devices, and cgroups.
- [4.4 Host-Guest Control Interface](4.4-host-guest-control-interface.md):
  Defines authenticated runtime operations and their session semantics.
