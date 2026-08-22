# Benchmark

The supported coordinator provides acceptance and diagnostic suites plus the
23-metric non-Python performance suite used by CI:

```bash
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm
python3 scripts/nvx.py benchmark \
	--suite performance \
	--backend kvm \
	--runs 5 \
	--virtfs-runs 3 \
	--output-dir build/benchmarks/linux-kvm
```

Select `cold-start`, `virtfs`, `shell-snapshot`, or `network-snapshot` to run
one performance workload. Historical Linux/KVM, Linux/MSHV, and Windows/WHP
CSVs are in `benchmarks/baselines`; rolling CI history is under its
`performance/` subdirectory.

[`NVX-METHODOLOGY.md`](../benchmarks/NVX-METHODOLOGY.md) defines the supported
metrics and measurement semantics. Python snapshots and Python-agent-dependent
console and prefetch workloads are excluded because the supported guest build
does not include a Python initramfs.

The OpenVMM benchmark coordinator is implemented in
`scripts/nvx_tools/benchmark.py` and exposed through the supported NVX CLI.
