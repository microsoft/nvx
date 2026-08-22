# Benchmark

The supported coordinator measures OpenVMM process launch through the
`ALPINE-MICROVM-BOOT-OK` marker, peak RSS, teardown, snapshot capture, and
restore:

```bash
python3 scripts/nvx.py benchmark --suite boot --backend whp
python3 scripts/nvx.py benchmark --suite e2e --backend kvm
```

Historical Linux/KVM and Windows/WHP CSVs are in `benchmarks/baselines`.
`benchmarks/NVX-METHODOLOGY.md` documents the inherited workload definitions.
The OpenVMM benchmark coordinator is implemented in
`scripts/nvx_tools/benchmark.py` and exposed through the supported NVX CLI.
