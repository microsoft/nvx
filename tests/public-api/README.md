# Public API snapshots

These snapshots record the public API of [`nvx`](../../nvx-rs/README.md) in
each configuration that adapters build on, those of the
[runtime adapter](../runtime-adapter/README.md), so that CI flags every change
to it, additions included.
[`check_public_api.py`](../../scripts/check_public_api.py) lists the API with
the cargo-public-api release that it pins, from the rustdoc JSON of the nightly
toolchain that it pins, and compares the listings with these files.

## Snapshots

| Configuration | `nvx` features | Targets | Snapshot |
| --- | --- | --- | --- |
| Protocol-only | none | `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` | `protocol.txt` |
| Host-only | `host` | `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` | `host.txt` |
| Guest-only | `guest` | `x86_64-unknown-linux-musl` | `guest.txt` |

Each line is a public item, such as a module, type, field, function, constant,
or implementation, with its full path. Blanket implementations, such as
`impl<T, U> Into<U> for T`, come from the standard library and are left out.
Auto-trait implementations, such as `Send` and `Sync`, and derived ones stay,
because adapters rely on them.

cargo-public-api leaves out the defaults of generic parameters: it lists
`pub type nvx::Result<T, E>` for `Result<T, E = Error>`. So the check reads the
rustdoc JSON that each listing builds and appends a line for each default of a
public item, such as
`generic default nvx::Result<E> = {"resolved_path":{"args":null,"path":"Error"}}`,
which records the default type as its rustdoc JSON without item IDs. A change
to a default then changes the snapshot too.

`NAME.txt` holds the items that every target of configuration `NAME` lists. A
target that lists more, such as an API for Windows handles, gets
`NAME.TARGET.txt` with those items, so that a difference between hosts shows
in the snapshots.

## Updating the snapshots

The `nvx-rs` CI job runs the check on Linux through
[`check-nvx-rs`](../../.github/actions/check-nvx-rs/action.yml), and fails when
the snapshots differ from the listings. When a change to `nvx` changes its
public API on purpose, update the snapshots and commit them with the change:

```bash
python3 scripts/check_public_api.py --install-tools
python3 scripts/check_public_api.py --update
```

`--install-tools` installs the pinned nightly toolchain, with the targets of
the configurations, and builds the pinned cargo-public-api with
`cargo install`; `--root DIRECTORY` installs it in `DIRECTORY/bin` instead of
Cargo's default. `--update` also removes the snapshots that no listing needs
any more. Then answer the adapter-contract question of the
[pull request template](../../.github/pull_request_template.md): a change to
the adapter contract carries a migration note and conformance cases.

The check exits with 0 when the snapshots match the listings, 1 when they
don't, and 2 when it cannot list the API, for example because a pinned tool is
missing.

## Why a snapshot

[microsoft/nvx#620](https://github.com/microsoft/nvx/issues/620) allows either
`cargo-semver-checks` against a Git baseline or a committed cargo-public-api
snapshot. A snapshot:

- flags every change, additions included, while `cargo-semver-checks` reports
  only the breaking changes that it has lints for;
- shows each change in the pull request's diff, and records the current API,
  the baseline of the next change, in the repository;
- accepts a deliberate breaking change through a reviewed update in the same
  pull request, while `cargo-semver-checks` accepts it only with a version
  bump, which `nvx` makes when it releases rather than in each change;
- needs neither Git history nor a second build of a baseline.

## Moving the pins

`CARGO_PUBLIC_API_VERSION` and `NIGHTLY_TOOLCHAIN` in the script pin the tools.
Each cargo-public-api release reads the rustdoc JSON of a range of nightly
toolchains, so move the pins together: pick a release, and the nightly
toolchain that `cargo-public-api/MINIMUM_NIGHTLY_RUST_VERSION_FOR_TESTS` names
at its tag, the oldest one that its tests run with. Then install the tools,
update the snapshots, whose rendering may change, and commit them with the
pins. CI caches the build of cargo-public-api under the hash of the script, so
a change to the script builds it again.
