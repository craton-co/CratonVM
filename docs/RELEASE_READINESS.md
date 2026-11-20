# Release readiness

First-party code is owned and maintained by [Craton Software Company](https://craton.com.ar)
under GPL version 2 only with the Classpath Exception 2.0. Workspace crates
inherit the SPDX expression and carry the project license and notices.
Vendored and ported code retains its original attribution and licenses.

## Registry blockers

The full workspace is not ready for a crates.io publish cycle:

- Publishable packages reference `publish = false` packages, including optional
  GPU and AWT dependencies. Registry resolution requires those packages to exist
  even when a feature is disabled. See [Cargo dependency rules](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html).
  Resolve their publication policy and verify
  the complete package graph before publishing dependent crates.
- `native-builtins` uses the workspace's patched rustls source. Consumers do not
  inherit a dependency's `[patch.crates-io]`. A reviewed, distributable dependency
  strategy must preserve those TLS changes in registry builds.
- Unpublished workspace dependencies must be published in order before full
  `cargo package`/`cargo publish --dry-run` can resolve registry versions.

`scripts/release-crates-dry-run.ps1 -MetadataOnly` checks company, license,
URLs, README, and legal files, and reports dependency blockers. Its default
preview lists the intended package order; `-Execute` refuses blocked selections.

## Required release checks

- Format, workspace build, Clippy, and workspace tests pass on the release commit.
- Default and synthetic-library configurations pass, with both execution policies
  compared against HotSpot 25 where applicable.
- Maintained documentation, source citations, and mdBook membership pass.
- Parser fuzz builds, pointer-representation Miri checks, and dependency/license
  checks pass; advisory CI settings are reviewed before making release claims.
- Every published crate builds and tests from its packaged copy with registry
  dependencies, without local files or private archives.
- Binary archives include the license and notices and pass smoke tests on every
  supported platform. Performance claims carry reproducible checksum-checked runs.

A green metadata check does not establish security isolation, production
suitability, full Java compatibility, or certification. See [security](../SECURITY.md)
and the [compatibility policy](book/src/reference/compatibility-policy.md).
