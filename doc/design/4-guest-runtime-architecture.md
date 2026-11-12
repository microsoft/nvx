# Guest Runtime Architecture

[Architecture documentation](README.md)

The NVX guest runtime turns one microVM into one workload sandbox, whose
isolation and resource boundary is the VM itself. For the life of the VM, the
guest's init, PID 1, runs the workload's executions as plain processes in one
root that the whole VM shares. In the
[layered sandbox root](4.1-guest-filesystem-and-image-model.md#411-layered-sandbox-root),
nvx-init assembles that root, sets one VM-wide hostname, and switches into the
root once; in the
[blockless managed root](4.1-guest-filesystem-and-image-model.md#413-blockless-managed-root),
the managed agent runs the executions in the guest's initramfs root. Neither
creates a namespace or a cgroup for an execution: each execution takes the
VM's workload credentials and runs in a process group of its own. The
architecture does not introduce pod infrastructure, a pause container, or a
guest-side service for creating arbitrary additional containers.

nvx-init's space is NVX's internal reference code that nvx-init implements,
not public `nvx` API. `nvx` keeps the contracts that a space reads, such as
the public `MappingTable`, the account of the host's mapping table, whose
ordered mappings the reference space's mapper binds. The
[guest-agent guidelines](../guest-agent-guidelines.md) give an init's
contract, with nvx-init as the worked example.

This chapter describes how the guest turns the virtual machine into an
isolated workload sandbox.

## Table of Contents

- [4.1 Guest Filesystem and Image Model](4.1-guest-filesystem-and-image-model.md):
  Explains the image, the writable upper, the root's assembly, live shares,
  and the host mapping table.
- [4.2 Guest Agents and Workload Model](4.2-guest-agents-and-workload-model.md):
  Defines the init, its executions, and its helpers.
- [4.3 Isolation and Resource Model](4.3-isolation-and-resource-model.md):
  Describes credentials, the VM-wide space, priority, and the one limit, the
  RAM upper's cap.
- [4.4 Host-Guest Control Interface](4.4-host-guest-control-interface.md):
  Defines authenticated runtime operations and their session semantics.
