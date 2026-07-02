# Release and crates.io readiness

This checklist is the public release-readiness gate for CratonVM source
releases, GitHub binary releases, and crates.io dry-runs. It complements the
root `RELEASING.md` process with the package and open-source checks that must be
true before a broad publish wave.

Current owner and license metadata:

- Owner: Craton Software Company.
- Source license: Apache-2.0.
- Root `LICENSE`: stock Apache License 2.0 text only.
- Project notice: root `NOTICE`.
- Third-party attribution: `THIRD-PARTY-NOTICES.md`, plus package-local notices
  where a published crate needs to carry attribution with the archive.

## Release readiness gate

Do not tag a public release or publish crates until these checks are complete:

1. Required CI checks are green on the release PR for the exact commit being
   tagged.
2. Advisory jobs are reviewed, with any known failures recorded in the release
   notes. Today this includes coverage and semantic difftest jobs while they
   remain explicitly advisory in `.github/workflows/coverage.yml` and
   `.github/workflows/ci.yml`.
3. Package contents are inspected with `cargo package --list` for every crate in
   the publish set.
4. `cargo package` and `cargo publish --dry-run` pass for every crate in
   dependency order.
5. No dry-run uses `--allow-dirty`, and no publish step uses `--no-verify`.
6. `Cargo.toml` workspace metadata still names Craton Software Company as the
   author and `Apache-2.0` as the license.
7. Each publishable crate has an appropriate `README.md`, `repository`,
   `homepage`, and versioned path dependency metadata.
8. Default features for publishable crates do not pull crates with
   `publish = false`.
9. Binaries exposed by packages have project-specific names. The intentional
   exception is the opt-in `java` launcher alias behind `java-bin-alias`.

## crates.io package set

The publish fence is per crate. A crate is withheld from crates.io by setting
`publish = false` in that crate's own `[package]` table.

Currently withheld:

- `cratonvm-native-awt`
- `cratonvm-jit-cuda`
- `cratonvm-gpu`
- `cratonvm-cuda-bridge`
- `cratonvm-fuzz`

Publishable after package and dry-run checks pass:

- `cratonvm-types`
- `cratonvm-reader`
- `cratonvm-native-api`
- `cratonvm-jit-api`
- `cratonvm-jit`
- `cratonvm-gc`
- `cratonvm-native-collections`
- `cratonvm-native-io`
- `cratonvm-classloading`
- `cratonvm-native-builtins`
- `cratonvm-jfr`
- `cratonvm-vm`
- `cratonvm-cli`
- `libcratonvm`
- `cratonvm-embed`
- `cratonvm-difftest`

Re-check the manifests before every release. If a new package is experimental,
platform-specific, or not ready for public support, add `publish = false` before
the release branch is tagged.

## Dry-run command checklist

Use `scripts/release-crates-dry-run.ps1` from the repository root to print or
run the package commands in dependency order.

Preview only:

```powershell
pwsh ./scripts/release-crates-dry-run.ps1
```

Run package-list, package, and publish dry-run checks:

```powershell
pwsh ./scripts/release-crates-dry-run.ps1 -Execute
```

Limit to a prefix of the dependency order while debugging:

```powershell
pwsh ./scripts/release-crates-dry-run.ps1 -Execute -Until cratonvm-jit-api
```

For each crate, retain the output from:

```text
cargo package -p <crate> --list
cargo package -p <crate>
cargo publish -p <crate> --dry-run
```

Inspect `cargo package --list` output for missing README/license/notice context,
generated files, fixtures accidentally excluded from packaged tests, local
scratch output, secrets, and internal-only material.

## License and notice consistency

Before publishing:

- Keep root `LICENSE` as the stock Apache License 2.0 text. Do not add project
  copyright lines to it.
- Keep Craton Software Company copyright and ownership statements in `NOTICE`,
  source headers, crate READMEs, or package metadata.
- Keep third-party attribution in `THIRD-PARTY-NOTICES.md` and ensure any crate
  containing third-party-derived material packages the relevant notice.
- Prefer SPDX expressions in source headers and Cargo manifests. The workspace
  default is `Apache-2.0`.
- If a file contains code derived from a third party, preserve the upstream
  copyright/license notice and do not imply Craton owns that upstream work.

## CI and release workflow references

Release owners should inspect these files before tagging:

- `.github/workflows/ci.yml`: required build, fmt, clippy, and test checks, plus
  advisory real-path and difftest gates.
- `.github/workflows/coverage.yml`: advisory coverage report generation.
- `.github/workflows/release.yml`: tag-triggered binary artifact build and
  GitHub Release creation.
- `.github/workflows/dco.yml`: DCO check for pull requests targeting `main`.
- `.github/CODEOWNERS`: default review ownership.
- `.github/pull_request_template.md`: release and package checklist prompts.

Any job described as advisory in docs must also be advisory in workflow YAML, and
any job described as required must be enforced by the repository's branch
protection rules.

## Repository readiness

Before a public open-source release:

- Confirm branch protection requires the intended CI checks and CODEOWNERS
  review for protected branches.
- Confirm `@craton-co/cratonvm-maintainers` has visibility and write/review
  access if CODEOWNERS uses that team.
- Confirm issue templates, `SECURITY.md`, `SUPPORT.md`, `CODE_OF_CONDUCT.md`,
  `GOVERNANCE.md`, `MAINTAINERS.md`, and `TRADEMARKS.md` are current.
- Confirm the release notes do not claim warning-free builds, complete coverage,
  or blocking advisory gates unless the current workflow state proves it.
- Confirm generated artifacts attached by `.github/workflows/release.yml` carry
  the expected binary names and include license/notice files where distribution
  format requires them.
