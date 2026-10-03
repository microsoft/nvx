# NVX microVM design

These documents describe the microVM machine profile implemented by the OpenVMM
submodule and the single-container sandbox filesystem and agent architecture
built on it. The implementation is the source of truth. Sections explicitly
marked **Proposed** retain production architecture and rationale that are not
yet implemented; they are not part of the current machine contract.

1. [Goals](design/goals.md)
2. [Configuration boundary](design/configuration-boundary.md)
3. [Cold boot](design/cold-boot.md)
4. [Machine and device ABI](design/machine-and-device-abi.md)
5. [Time ABI](design/time-abi.md)
6. [Sandbox filesystem and agent architecture](design/sandbox-filesystem-and-agent-architecture.md)
7. [Ubuntu guest support](guests/ubuntu-guest.md)
8. [Snapshot and restore](design/snapshot-and-restore.md)
9. [Snapshot sharing and host storage](design/snapshot-sharing-and-host-storage.md)
10. [Host attachment model](design/host-attachment-model.md)
11. [Concurrency and trust boundaries](design/concurrency-and-trust-boundaries.md)
12. [Validation](design/validation.md)
13. [Current limits](design/current-limits.md)
14. [Remaining production work](design/remaining-production-work.md)
15. [Integrated implementation status](design/integrated-implementation-status.md)
16. [Component responsibility map](design/code-ownership-map.md)
17. [Copilot-driven adversarial testing](design/copilot-adversarial-testing.md)
