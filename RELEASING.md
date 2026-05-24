# Releasing CratonVM

This document describes how to cut a new release of CratonVM.

## 1. Versioning

- We follow [Semantic Versioning](https://semver.org/): `MAJOR.MINOR.PATCH`.
  - `MAJOR` — incompatible public API changes.
  - `MINOR` — backward-compatible feature additions.
  - `PATCH` — backward-compatible bug fixes.
- We follow [Keep a Changelog](https://keepachangelog.com/) for `CHANGELOG.md`:
  every release has its own dated section, and unreleased work accumulates
  under a `[Unreleased]` heading at the top.
- The workspace version lives in `[workspace.package]` in the root
  `Cargo.toml` (`version = "X.Y.Z"`). Every workspace member inherits it via
  `version.workspace = true`.

## 2. Cutting a release

1. **Open a release PR** from a branch named e.g. `release/X.Y.Z` that:
   - Bumps `version = "X.Y.Z"` in `[workspace.package]` of root `Cargo.toml`.
   - Promotes the `[Unreleased]` section in `CHANGELOG.md` to
     `[X.Y.Z] - YYYY-MM-DD` (use today's UTC date), and adds a fresh empty
     `[Unreleased]` block above it with the standard subsections
     (`### Added`, `### Changed`, `### Fixed`, etc.).
   - Updates the compare-links at the bottom of `CHANGELOG.md`:
     - Change the previous `[Unreleased]` link to
       `[Unreleased]: https://github.com/craton-co/cratonvm/compare/vX.Y.Z...HEAD`.
     - Add `[X.Y.Z]: https://github.com/craton-co/cratonvm/compare/vPREV...vX.Y.Z`.

2. **Wait for CI green** on the PR. `.github/workflows/ci.yml` runs
   `cargo fmt --check`, `cargo build`, `cargo clippy`, and `cargo test`
   across the workspace on Linux and Windows. All checks must pass.

3. **Merge the PR** into `main` (squash or merge — match repo policy).

4. **Tag the release commit** on `main`:

   ```sh
   git checkout main
   git pull
   git tag -a vX.Y.Z -m "Release X.Y.Z"
   git push origin vX.Y.Z
   ```

5. **Build and publish the release artifacts.** A release-workflow
   template lives at `.github/_disabled-workflows/release.yml`
   (currently disabled — see RELEASING.md §2; move it into
   `.github/workflows/` to enable automatic tag-triggered releases).
   Until then, build and upload the artifacts manually:
   - Build `cratonvm-cli` for `x86_64-unknown-linux-gnu`,
     `x86_64-pc-windows-msvc`, and `aarch64-apple-darwin`.
   - Attach the artifacts to a new GitHub Release for the tag.

## 3. Publishing to crates.io (when ready)

The workspace currently sets `publish = false` at `[workspace.package]`,
so nothing is pushed to crates.io. To start publishing:

1. **Decide a per-crate publish policy.** Crates that are path-only
   integration glue or contain bundled assets — `cuda-bridge`, `jit-cuda`,
   `native-awt`, internal `fuzz` targets — should stay `publish = false`.
2. **Override at the crate level** for each publishable crate by adding
   `publish = true` to its own `[package]` table (this overrides the
   workspace default).
3. **Convert path-only deps to versioned form.** crates.io rejects pure
   `{ path = "..." }` dependencies on publish. Bump each inter-crate
   dependency to `{ path = "...", version = "X.Y.Z" }` so local builds
   stay path-resolved while the published metadata has a version.
4. **Publish in dependency order**, leaves first. A typical order is:
   `cratonvm-types` → `cratonvm-reader` → `cratonvm-jit-api` →
   `cratonvm-native-api` → `cratonvm-gc` → `cratonvm-classloading` →
   `cratonvm-jit` → `cratonvm-jfr` → `cratonvm-native-collections` →
   `cratonvm-native-io` → `cratonvm-native-builtins` → `cratonvm-vm` →
   `cratonvm-cli`.
5. **Dry-run each crate first**, then publish:

   ```sh
   cargo publish --dry-run -p <crate>
   cargo publish           -p <crate>
   ```

   Do **not** reach for `--no-verify` or `--allow-dirty` to paper over
   failures — diagnose and fix the underlying cause.

## 4. Post-release

- Confirm the GitHub Release page at
  `https://github.com/craton-co/cratonvm/releases/tag/vX.Y.Z` exists and
  has all three platform artifacts attached.
- If `README.md` carries a "latest release" badge or download link,
  update it to point at `vX.Y.Z`.
- Announce the release (changelog highlights, blog post, etc.) as
  appropriate.

## 5. Hotfixes

For an urgent fix on top of an already-shipped release:

1. Branch off the release tag: `git checkout -b hotfix/X.Y.(Z+1) vX.Y.Z`.
2. Apply the minimal fix and update `CHANGELOG.md` under `[Unreleased]`.
3. Bump the patch version (`X.Y.Z` → `X.Y.(Z+1)`) in root `Cargo.toml`.
4. Open a PR targeting `main`; wait for CI; merge.
5. Tag `vX.Y.(Z+1)` on the merge commit and push — the release workflow
   builds and publishes binaries.
6. If the affected crates are published to crates.io, repeat section 3
   for the patched crates (`cargo publish --dry-run` first).
