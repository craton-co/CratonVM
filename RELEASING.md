# Releasing CratonVM

This document describes how to cut a new release of CratonVM.

> **Current readiness note:** the repository is not ready for a public release
> tag or broad crates.io publish wave until the release-readiness blockers from
> the most recent full-scoped review are resolved — `cargo fmt --all -- --check`
> alone still fails workspace-wide. Do not infer release readiness from local
> source-tree package checks alone; verify the exact release commit with the
> gates below.

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
- Note that versions appear in **two** places, and both track the same
  `X.Y.Z`: (1) each member's *package* version is inherited from the
  workspace via `version.workspace = true` (so bumping
  `[workspace.package].version` in the root `Cargo.toml` bumps every crate);
  and (2) each inter-crate path dependency **also** carries an explicit
  `version = "X.Y.Z"` alongside its `path = "..."` (e.g.
  `cratonvm-reader = { path = "../reader", version = "0.3.0" }`), which local
  builds ignore but crates.io requires (see §3.3). When you bump the
  workspace version, update these explicit dep `version =` literals to match.

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

2. **Wait for required CI green** on the PR. `.github/workflows/ci.yml` runs
   `cargo fmt --check`, `cargo build`, `cargo clippy`, and `cargo test`
   across the workspace on Linux and Windows. Required checks must pass on the
   exact commit being tagged. Coverage, semantic difftest, real-path smoke, and
   fuzz-smoke jobs are advisory until their `continue-on-error` settings are
   intentionally removed.

3. **Merge the PR** into `main` (squash or merge — match repo policy).

4. **Tag the release commit** on `main`:

   ```sh
   git checkout main
   git pull
   git tag -a vX.Y.Z -m "Release X.Y.Z"
   git push origin vX.Y.Z
   ```

5. **Let the release workflow build and publish the artifacts.** Pushing a
   `vX.Y.Z` tag triggers `.github/workflows/release.yml` (active; it runs
   `on: push: tags: ['v*']`). It builds `cratonvm-cli` for
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, and
   `aarch64-apple-darwin` and attaches the artifacts to a GitHub Release for
   the tag. No manual artifact build is required; just watch the workflow run
   complete on the Actions tab.

## 3. Publishing to crates.io (when ready)

Publish gating is **per-crate**, not workspace-wide. There is no
`publish = false` at `[workspace.package]`; instead, the immature /
non-shippable crates each carry their own `publish = false` in their
`[package]` table. As of this writing the crates fenced off with
`publish = false` are:

- `cratonvm-native-awt` - headless AWT/Swing/Java2D peers (immature).
- `cratonvm-jit-cuda` - Java-bytecode-to-PTX lowering (GPU offload, opt-in/immature).
- `cratonvm-gpu` - build-time GPU-offload annotation sources.
- `cratonvm-cuda-bridge` - thin CUDA Driver API bridge (GPU offload, opt-in/immature).
- `cratonvm-fuzz` - the standalone libFuzzer harness (nightly-only internal target, never published).

Everything else is intended to be publishable only after package-list,
package-copy, and dry-run checks pass for that exact crate:
`cratonvm-types`, `cratonvm-reader`, `cratonvm-native-api`,
`cratonvm-jit-api`, `cratonvm-jit`, `cratonvm-gc`,
`cratonvm-native-collections`, `cratonvm-native-io`,
`cratonvm-classloading`, `cratonvm-native-builtins`, `cratonvm-jfr`,
`cratonvm-vm`, `cratonvm-cli`, `libcratonvm`, `cratonvm-embed`, and
`cratonvm-difftest`.

Do not start a broad publish wave while default features still pull
unpublished crates. In particular, `cratonvm-vm` defaults include `awt`,
which reaches `cratonvm-native-awt`; downstream packages such as
`cratonvm-cli`, `libcratonvm`, and `cratonvm-embed` inherit that edge through
`cratonvm-vm`. Split or disable those default-feature edges before publishing
the dependent crates, and verify with `cargo package` / `cargo publish
--dry-run`.

Also re-check packaged-copy tests and crates.io dependency availability before
publishing. A source-tree test pass is not enough: packaged archives can exclude
fixtures, and higher-level crates cannot dry-run until their versioned
dependencies are already available from crates.io or are otherwise split out of
the publish graph.

To publish:

1. **Confirm the publish gates.** Verify the GPU/AWT crates and `cratonvm-fuzz` above
   still carry `publish = false`, and that no newly-added immature crate should
   join that list. Set `publish = false` on a crate's own `[package]` table to
   keep it off crates.io.
2. **Confirm package metadata.** Each publishable crate should inherit or set
   the Craton Software Company author, `Apache-2.0` license, repository,
   homepage, documentation, README, and versioned path-dependency metadata.
3. **Path deps already carry versions.** Each inter-crate dependency is already
   in the versioned form `{ path = "...", version = "X.Y.Z" }` (see §1) — local
   builds resolve by path while the published metadata carries the version that
   crates.io requires. Just keep the `version =` literals in lockstep with the
   workspace version when you bump it.
4. **Publish in dependency order**, leaves first. Re-check this order against
   the manifests before a release; a typical order is:
   `cratonvm-types` ->
   `cratonvm-reader` ->
   `cratonvm-native-api` ->
   `cratonvm-jit-api` ->
   `cratonvm-jit` ->
   `cratonvm-gc` ->
   `cratonvm-native-collections` ->
   `cratonvm-native-io` ->
   `cratonvm-classloading` ->
   `cratonvm-native-builtins` ->
   `cratonvm-jfr` ->
   `cratonvm-vm` ->
   `cratonvm-cli` ->
   `libcratonvm` ->
   `cratonvm-embed` ->
   `cratonvm-difftest`.
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
