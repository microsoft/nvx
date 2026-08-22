# NVX OpenVMM distribution

This repository builds and packages a small Alpine Linux guest for OpenVMM's
`microvm` machine. It keeps the exact Linux and Alpine source pins, complete
kernel patch series, guest-owned sources, build tooling, and benchmark
workloads in Git. Large upstream source trees are verified and materialized
under ignored build/cache directories only when building or preparing a
release. OpenVMM is the only private component and is pinned as the `openvmm`
Git submodule.

Access to `https://github.com/nanvix/openvmm` is required to initialize or
update the submodule. Cloning this repository does not grant access to it.

## Documentation

- [Setup](doc/setup.md)
- [Continuous integration](doc/ci.md)
- [Build](doc/build.md)
- [Run](doc/run.md)
- [Benchmark](doc/benchmarks.md)
- [Package and source delivery](doc/distribution.md)
- [Project structure](doc/project-structure.md)
