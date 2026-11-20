# Releasing CratonVM

CratonVM is experimental. Public distribution requires the checks in
[release readiness](docs/RELEASE_READINESS.md) on the exact release commit.
A source-tree build alone does not establish a publishable crate.

## Version and source

The version is inherited from `[workspace.package]` in `Cargo.toml`. Update
inter-crate dependency versions with the workspace version. Develop changes on
`dev`, review and integrate the release candidate into the release branch,
and tag only the verified commit. Release notes belong to the GitHub release;
public manuals describe current behavior.

## GitHub artifacts

Pushing a `v*` tag triggers `.github/workflows/release.yml`. It builds Linux
x86-64, Windows x86-64, and macOS AArch64 artifacts. Each archive carries
`LICENSE`, `NOTICE`, and `THIRD-PARTY-NOTICES.md`. Verify installation and a
real-JDK smoke run on each advertised platform before publishing the release.
The workflow's platform matrix does not establish runtime certification.

## crates.io

Craton Software Company ([craton.com.ar](https://craton.com.ar)) owns and
maintains the first-party code. Its SPDX license expression is
`GPL-2.0-only WITH Classpath-exception-2.0`; third-party code retains its notices.

```powershell
./scripts/release-crates-dry-run.ps1 -MetadataOnly
./scripts/release-crates-dry-run.ps1
./scripts/release-crates-dry-run.ps1 -Execute -Until cratonvm-types
```

The default command previews packaging and publish dry-runs in dependency order;
`-Execute` performs dry-runs, never a real publish. Every selected crate must pass
`cargo package` and `cargo publish --dry-run` without bypassing verification.
Publish dependencies first, then their dependents, using an authorized registry
account. Confirm crate ownership separately in that account.

Current blockers are explicit in [release readiness](docs/RELEASE_READINESS.md):
withheld optional packages and the local rustls patch prevent declaring the
entire workspace ready for registry consumption. Fixing metadata does not
resolve these dependency and packaged-copy constraints.
