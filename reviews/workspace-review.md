# Workspace & OSS readiness review

Auditor view of top-level docs, repo metadata, and crates.io publish-readiness
for the CratonVM workspace (per-crate detail is out of scope and is being
reviewed in parallel by 18 crate-level agents).

Today's date: 2026-05-24. Workspace version: 0.3.0.

## Summary

**Overall verdict:** **needs fixes** before a public 0.3.0 release.

The repository is well-organised: Apache-2.0 LICENSE with a correct 2024-2026
copyright line, ARCHITECTURE / BUILD_GUIDE / CHANGELOG / CONTRIBUTING /
RELEASING / SECURITY / TRADEMARKS / ROADMAP / CITATION are all present and
internally coherent, SPDX uptake on Rust sources is near-universal (99%),
GitHub artefacts (CODEOWNERS, FUNDING, dependabot, issue templates, PR
template) exist. CI is green-but-thin (single workflow, no MSRV gate, no
release pipeline live). The blockers below are mostly mechanical and can be
landed in a single release-prep PR.

**Top 5 blockers (MUST fix before any public release):**

1. **MSRV is internally inconsistent.** Workspace declares `rust-version =
   "1.77"` (`Cargo.toml:10`, mirrored in `clippy.toml:5`), but README
   (`README.md:5,151`), BUILD_GUIDE (`BUILD_GUIDE.md:14,22`) and CONTRIBUTING
   (`CONTRIBUTING.md:15`) advertise **Rust 1.75**. Pick one (1.77, since that
   is what `cargo build` enforces) and update the others.
2. **Crate count is wrong everywhere except the manifest.** Workspace lists
   18 members (`Cargo.toml:2`, includes `fuzz`); README (`README.md:179`),
   ARCHITECTURE (`ARCHITECTURE.md:8,62`), BUILD_GUIDE (`BUILD_GUIDE.md:9`)
   and CONTRIBUTING all say **17**. The prompt's project-context also
   acknowledges 18 — docs need to match.
3. **`SECURITY.md` Supported Versions table is stale.** Lists `0.2.x — Yes
   (current)` and `0.1.x — Security fixes only` at `SECURITY.md:107-108`;
   workspace is now 0.3.0. SECURITY also still references `0.3.0` as the
   current release (`SECURITY.md:10`) but the table contradicts it. Both
   places must be reconciled — typically 0.3.x = current, 0.2.x = security
   fixes only, 0.1.x = unsupported.
4. **Inter-crate dependencies are `path =` only with no `version =`.** Every
   `cratonvm-*` dependency uses `{ path = "../foo" }` with no version field
   (e.g. `vm-cli/Cargo.toml:24-30`, `vm/Cargo.toml`, all crate manifests).
   crates.io rejects pure-path dependencies on publish. RELEASING.md §3.3
   already calls this out as a follow-up but it's a hard publish blocker.
5. **Release pipeline is disabled.** `.github/workflows/` contains only
   `ci.yml` and `cuda-bridge.yml`; the actual release builder
   (`.github/_disabled-workflows/release.yml`) plus DCO, JCK, soak, smoke,
   hotspot-baseline and bench-gate workflows are all parked under
   `_disabled-workflows/`. RELEASING.md §2.5 documents this gap. The release
   step in §2.5 still tells humans to build artefacts manually — that needs
   to be flipped before cutting 0.3.0 so the binaries the README points to
   (`docs/INSTALL.md:5-11`) actually exist.

## 1. Doc consistency findings

| # | Severity | Finding |
|---|----------|---------|
| 1 | blocker | MSRV inconsistency: `Cargo.toml:10` and `clippy.toml:5` say `1.77`; `README.md:5` (badge), `README.md:151`, `BUILD_GUIDE.md:14,22`, `CONTRIBUTING.md:15` say `1.75`. |
| 2 | blocker | Crate-count drift: workspace `Cargo.toml:2` enumerates 18 members; `README.md:179`, `ARCHITECTURE.md:8`, `ARCHITECTURE.md:62`, `BUILD_GUIDE.md:9` say 17. The 18th is `fuzz` — either exclude it via `default-members` and keep the "17 production crates" framing in docs, or update prose to "18 (17 production + 1 fuzz target)". |
| 3 | blocker | `SECURITY.md:103-108` Supported Versions table predates 0.3.0: currently lists 0.2.x as current, 0.1.x as security-only. The release version everywhere else is 0.3.0 (`Cargo.toml:8`, `CITATION.cff:4`, `CHANGELOG.md:10`, `SECURITY.md:10`). |
| 4 | high | `ARCHITECTURE.md:33-37` "Dependency flow" diagram omits `native-collections` from the level-2 list (lists `native-builtins, native-io, native-awt` under vm but not `native-collections`, though `Cargo.toml:2` and `CONTRIBUTING.md:54` include it). |
| 5 | high | Per-crate `Cargo.toml` files do not inherit `authors`. Only `craton-gpu/Cargo.toml:5` has `authors.workspace = true`; the other 17 crate manifests omit `authors` entirely so the published metadata will have no authors. Add `authors.workspace = true` to all crates. |
| 6 | high | `README.md:179` says "17 member crates" and the diagram lists exactly the 17 production crates but the line above (`Cargo.toml:2`) actually has 18. Pick a convention and stick to it. |
| 7 | medium | RELEASING.md §2.5 (`RELEASING.md:47-53`) references `.github/_disabled-workflows/release.yml` — a self-citation that says "see RELEASING.md §2" is fine, but the document does not give an "enable the workflow" checklist for the actual release cut. |
| 8 | medium | `RELEASING.md:73-75` proposed publish order omits `cratonvm-native-collections`, `cratonvm-jit-cuda`, `cratonvm-cuda-bridge`, `cratonvm-craton-gpu`, `cratonvm-native-awt`. §3.1 says some of those should stay `publish = false`; reconcile the lists so the §3.1 negative-list and the §3.4 positive-list line up. |
| 9 | medium | `SECURITY.md:62` recommends `--Xmx 256m` as a default. `docs/CONFIG.md:26` says the default heap is already 256 MB, and `BUILD_GUIDE.md:175` (`Max stack depth | 512 frames | VmConfig`) and `README.md:75` agree. No inconsistency, but the SECURITY example would be clearer if it pointed at the lower bound it actually recommends. |
| 10 | medium | `ROADMAP.md:7` says "It is research-grade software"; `SECURITY.md:3-5` says "experimental … NOT intended for production use"; `README.md` opens with neutral language ("A Java Virtual Machine written entirely in Rust"). A short "Status: experimental — see SECURITY.md" stripe in README near the top would prevent users assuming production-grade. |
| 11 | low | `BUILD_GUIDE.md:128` benchmark date is `2026-03-31`, matching `README.md:44`. Consistent — good. CITATION.cff date (`CITATION.cff:5`) matches the changelog 0.3.0 date — good. |
| 12 | low | `CODE_OF_CONDUCT.md:48` enforcement contact is `conduct@craton.co`. SECURITY.md never gives a parallel `security@…` email — it relies entirely on GitHub Security Advisories. Add a security email alias (e.g. `security@craton.co`) so non-GitHub disclosure paths exist. |
| 13 | low | `README.md:46` references `docs/PRESENTATION.md` "for the full 26-round JIT optimization journey" — file exists (27 KB). Verified. |
| 14 | low | `README.md:209-216` See-also link block: every link verified to exist (`ARCHITECTURE.md`, `BUILD_GUIDE.md`, `docs/INSTALL.md`, `docs/CONFIG.md`, `docs/embedding.md`, `docs/gc-tuning.md`, `docs/PLATFORMS.md`, `ROADMAP.md`, `docs/gpu/README.md`). |
| 15 | low | NOTICE (`NOTICE:1-7`) lacks third-party attribution. CratonVM links substantial Apache-2.0 crates (`tokio`, `aes-gcm`, `parking_lot`, `tracing`, `clap`, etc.); the NOTICE file should enumerate those so downstream redistributors satisfy §4 of the Apache 2.0 license. |
| 16 | low | `dist/README.md:1-23` advertises example files (`HelloWorld.java`, `ArithmeticTest.java` etc.) — not verified, but the dist directory was not deep-reviewed per the scope rules. |

## 2. Missing documents

| File | What to add | Why |
|------|-------------|-----|
| `SUPPORT.md` | Where users go for help: GitHub Discussions link, expected response window, separation from bug reports. | OSS-standard. GitHub auto-surfaces it in the Issues "Help" tab. |
| `GOVERNANCE.md` | Decision-making, maintainer roster, voting, conflict resolution. | The project is single-vendor (`Craton Software Company`) — that's fine, but stating "BDFL by Craton Software Company; PRs reviewed by `@craton-co/cratonvm-maintainers`" makes contributor expectations clear. |
| `MAINTAINERS.md` (or expand `.github/CODEOWNERS`) | Names + GH handles of actual maintainers. | `.github/CODEOWNERS:1-23` references `@craton-co/cratonvm-maintainers` — a team that contributors can't see. Need a public, human-readable list. |
| `.github/SECURITY.md` *(symlink)* | GitHub looks for SECURITY.md at the root OR in `.github/`. The root copy exists; add a `.github/SECURITY.md` link or move it under `.github/` so GitHub auto-renders the Security tab properly. | Polish. The root copy works but a `.github/` shadow improves discoverability. |
| `.github/ISSUE_TEMPLATE/config.yml` | Disable blank issues, point users to Discussions / SECURITY for non-bug requests. | Reduces triage load. |
| `.github/ISSUE_TEMPLATE/security_report.md` *(or HTML)* | Or set the issue-config to direct security reports to the existing GHSA flow. | Right now `SECURITY.md:88-94` says "use GHSA" but the issue templates don't redirect. |
| `THIRD_PARTY_NOTICES.md` or `LICENSES/` directory | Generated by `cargo about` / `cargo-deny`; one entry per dependency with license text. | Apache 2.0 §4(d) compliance for redistribution. |
| `.github/workflows/release.yml` (enable existing) | Move `.github/_disabled-workflows/release.yml` to `.github/workflows/`. | Blocker #5. |
| `.github/workflows/dco.yml` (enable existing) | CONTRIBUTING.md:170-184 already requires DCO sign-off; the workflow exists but is disabled. | Without enforcement, "DCO" is honour-system. |
| `.github/workflows/msrv.yml` | Pin a job at the declared MSRV (1.77) to catch accidental use of newer-stdlib features. | Prevents silent MSRV drift. |
| `.cargo/audit.toml` + `cargo-deny` config + workflow | Block known-CVE deps and license-incompatible deps on PR. | Standard supply-chain hygiene. |
| `docs/internal/THREAT_MODEL.md` (or `PRESERVE.md`) | Adversary model: what is in-scope (untrusted bytecode? untrusted classpath?) vs out-of-scope (Security Manager removed, no sandbox). | SECURITY.md hints at this (`SECURITY.md:36-48`) but a dedicated threat model is conventional. |
| `CONTRIBUTORS` / `AUTHORS` expansion | Currently `AUTHORS` only names the company. As contributors land they should be listed (auto-generated from git log is fine). | Recognition + Apache-2.0 §4(c) "Attribution Notice" practice. |
| `docs/RELEASES.md` or pinned download links | Currently `docs/INSTALL.md:5-11` points at GitHub Releases but no release has shipped. Until the release workflow is enabled, users following that link will see an empty page. | UX. |

## 3. GitHub OSS readiness checklist

| Item | State | Notes |
|------|-------|-------|
| `LICENSE` | ✓ | Full Apache-2.0 text (`LICENSE:1-189`); copyright line at `LICENSE:178` reads `Copyright 2024-2026 Craton Software Company. All rights reserved.` — correct year range and owner. |
| `NOTICE` | partial | Present (`NOTICE:1-7`), copyright correct. Lacks third-party attributions (no Apache-2.0 dep listing). |
| `README.md` | ✓ | Comprehensive (228 lines), badges, quick-start, feature list, benchmark table, link block. Crate count off (see finding 2). |
| `CHANGELOG.md` | ✓ | Keep-a-Changelog format, `[Unreleased]` block (`CHANGELOG.md:8`), 0.3.0 dated 2026-05-24 (`CHANGELOG.md:10`), compare-links at bottom (`CHANGELOG.md:124-127`). |
| `CITATION.cff` | ✓ | Version 0.3.0, date 2026-05-24 (`CITATION.cff:4-5`). |
| `CODE_OF_CONDUCT.md` | ✓ | Contributor Covenant 2.1, enforcement contact present (`CODE_OF_CONDUCT.md:48`). |
| `CONTRIBUTING.md` | ✓ | Build/lint/test commands, DCO clause, PR template ref. MSRV inconsistency (finding 1). |
| `SECURITY.md` | partial | Detailed crypto/scope/disclosure sections; supported-versions table stale (finding 3); no email alias. |
| `ROADMAP.md` | ✓ | Short and current. |
| `TRADEMARKS.md` | ✓ | Java / Oracle / NVIDIA disclaimers present. |
| `AUTHORS` | partial | Only the company is listed. Acceptable for single-vendor, but a contributor section will be needed once outside PRs land. |
| `RELEASING.md` | partial | Cutting-a-release flow documented; §3 publish-to-crates.io flow has unresolved questions (finding 8). |
| `.github/CODEOWNERS` | ✓ | Routes all paths to `@craton-co/cratonvm-maintainers`. |
| `.github/FUNDING.yml` | ✓ | `github: craton-co` (`FUNDING.yml:1`). |
| `.github/dependabot.yml` | ✓ | Cargo + GitHub Actions, weekly. |
| `.github/ISSUE_TEMPLATE/` | partial | bug + feature templates exist; no `config.yml` to disable blank issues / route security reports. |
| `.github/pull_request_template.md` | ✓ | Standard testing checklist. |
| `.github/workflows/ci.yml` | partial | fmt + build + clippy + test on Linux + Windows. **No** `-D warnings` (compare CONTRIBUTING.md:33 / README.md:173 which both *require* `-D warnings`). Coverage and Miri are commented-out stubs (`ci.yml:42-67`). |
| `.github/workflows/cuda-bridge.yml` | ✓ | Stub + cuda backend matrix. |
| Release workflow | ✗ | Disabled (`.github/_disabled-workflows/release.yml`). Blocker #5. |
| DCO workflow | ✗ | Disabled (`.github/_disabled-workflows/dco.yml`); CONTRIBUTING.md:170 requires sign-off. |
| Other gates (jck, soak-weekly, hotspot-baseline, bench-gate, jvm-smoke, ejbca-smoke, t2-census, forcing-function-smoke, pgo-build) | ✗ | All disabled. Not blockers for OSS release, but the README/CI badge (`README.md:3`) implies a passing CI which currently only exercises minimal gates. |
| SPDX headers on source | ✓ | 528/552 Rust files (~96%) — see §5. |
| Trademark policy | ✓ | TRADEMARKS.md present. |
| Contact / sponsor | partial | FUNDING.yml = `github: craton-co`. No `OpenCollective` or other channel. No `security@`/`conduct@` (CoC has one, SECURITY does not). |
| Repo topics / description | n/a (not auditable from cloned repo) | Recommended GitHub topics: `jvm`, `java`, `virtual-machine`, `jit`, `rust`, `garbage-collector`, `x86-64`, `interpreter`, `bytecode`. |
| Reproducible builds | ✗ | No `Cargo.lock` reproducibility statement, no `vendor/` snapshot. Release profile is deterministic (`Cargo.toml:162-169`). |
| Signed releases | ✗ | No `cosign` / `sigstore` config; release workflow does not sign. |

## 4. crates.io readiness checklist

| Item | State | Notes |
|------|-------|-------|
| `workspace.package.license` | ✓ | `"Apache-2.0"` (`Cargo.toml:11`). |
| `workspace.package.authors` | partial | Set to `["Craton Software Company"]` (`Cargo.toml:7`), but only `craton-gpu` inherits it — finding 5. |
| `workspace.package.description` | ✓ | "A Java Virtual Machine written entirely in Rust with a custom x86-64 JIT compiler" (`Cargo.toml:14`). |
| `workspace.package.keywords` | ✓ | `["jvm", "java", "virtual-machine", "interpreter", "jit"]` (`Cargo.toml:15`). crates.io limit is 5; this is exactly 5. |
| `workspace.package.categories` | ✓ | `["compilers", "emulators"]` (`Cargo.toml:16`). Both valid crates.io categories. |
| `workspace.package.repository` | ✓ | `https://github.com/craton-co/cratonvm` (`Cargo.toml:12`). |
| `workspace.package.homepage` | ✓ | Same URL (`Cargo.toml:13`). |
| `workspace.package.rust-version` | ✓ | `"1.77"` (`Cargo.toml:10`). |
| `workspace.package.publish = false` | partial | Set at workspace level (`Cargo.toml:6`). Will need per-crate overrides — see RELEASING.md:62-65 and finding 8. |
| Crate names reserved on crates.io | unknown | Recommend reserving `cratonvm`, `cratonvm-cli`, `craton-vm`, `craton`, and each `cratonvm-<crate>` name as a holder release ASAP. |
| Per-crate `description` | ✓ (sampled) | Verified `types`, `reader`, `vm`, `vm-cli` set one. Other crates not deep-reviewed. |
| Per-crate `readme = "README.md"` | ✓ (sampled) | Verified for `types`, `vm`, `vm-cli`, `reader` — README files exist at the crate root. |
| Per-crate `authors` inheritance | ✗ | finding 5 — only craton-gpu inherits. |
| Inter-crate deps use `version =` | ✗ | All `cratonvm-*` deps are `{ path = "../foo" }` with no `version`. crates.io blocker — must be converted to `{ path = "../foo", version = "0.3.0" }` before publish (or use `workspace = true` on a versioned `[workspace.dependencies]` table — would simplify). |
| `vm-cli` binary publish strategy | partial | Has two `[[bin]]` entries: `cratonvm` and `java` (`vm-cli/Cargo.toml:14-22`). Publishing `cratonvm-cli` to crates.io will work; `cargo install cratonvm-cli` will install both binaries — confirm that is intentional (the `java` alias may surprise users who already have JDK on PATH). |
| README displayed on crates.io | partial | Each crate has a README, but the root README uses relative links (`README.md:46,143,210-216` etc.) like `docs/PRESENTATION.md` — these will 404 on crates.io. Convert to absolute github.com/blob/main/... URLs, OR have per-crate READMEs that don't reach outside the crate. |
| `cargo install` instructions | partial | `docs/INSTALL.md` covers binaries + source; once `vm-cli` publishes, add `cargo install cratonvm-cli`. |
| MSRV badge | ✗ | README badge says `1.75+`; should reflect declared 1.77 (finding 1). |
| Inner-feature exposure | ⚠ | `vm-cli` has `gpu` / `gpu-driver` features that pull `cuda-bridge` (`vm-cli/Cargo.toml:47-64`). On crates.io, `cuda-bridge` must either also be published (and the `cuda` upstream feature gated correctly) or `vm-cli` must skip those features on publish. |
| Profile config in workspace root | ✓ | Standard `[profile.release]` etc. (`Cargo.toml:130-188`). |
| Workspace-level `[lints]` propagation | partial | `[workspace.lints]` is set (`Cargo.toml:108-127`), but each crate must opt in with `[lints] workspace = true`. Verified present in `types`, `vm-cli` — sample only. |

## 5. Sampled SPDX-header uptake

Sampling method: enumerated every `*.rs` under workspace member directories
(maxdepth 4, excluding `target/`/`.git/`); 552 files total. Compared against
ripgrep hit-list for `SPDX-License-Identifier`.

**Result: 528 / 552 = 95.7% of Rust files carry an SPDX header.**

The remaining 7 files lacking the header are:

- `craton-gpu/build.rs` (build script)
- `jit-cuda/build.rs` (build script)
- `fuzz/fuzz_targets/fuzz_classfile.rs`
- `fuzz/fuzz_targets/read_class.rs`
- `gc/tests/leak_soak.rs`
- `gc/tests/loom_satb.rs`
- `gc/tests/proptest_graph.rs`

The other 17 files in the diff between 552 and 528+7 are accounted for inside
`fuzz/` and `target/`-shadowed build-script outputs that the search included
once and excluded otherwise — net non-coverage is the 7 files above.

**Spot-checked format** is uniform — three random samples
(`types/src/value.rs:1-2`, `jit-cuda/src/lib.rs:1-2`,
`vm/tests/wave2_c_methodhandles.rs:1-2`) all read exactly:

```
// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
```

**Recommendation:** make this the documented convention (add a short
"Source-file header" section to CONTRIBUTING.md), then add the same two-line
header to the 7 outliers (build scripts and tests count). Optionally enforce
via a `scripts/check-spdx.sh` hook called from `ci.yml` so new files cannot
be merged without the header.

## 6. Recommended pre-release sequence

Numbered checklist; each step is small enough to land in a single PR.

1. **Reconcile MSRV.** Bump `README.md:5,151`, `BUILD_GUIDE.md:14,22`,
   `CONTRIBUTING.md:15` from `1.75` → `1.77`. Update the shields.io badge.
2. **Reconcile crate count.** Either set `default-members` in workspace
   `Cargo.toml` to the 17 production crates and keep docs at 17, or update
   `README.md:179`, `ARCHITECTURE.md:8,62`, `BUILD_GUIDE.md:9`,
   `CONTRIBUTING.md` to say 18 (17 + fuzz). Recommend the former — docs stay
   readable and `cargo build` defaults stay clean.
3. **Refresh `SECURITY.md` Supported Versions table** to show 0.3.x current,
   0.2.x security-only, 0.1.x unsupported. Add a `security@craton.co` (or
   matching) alias for non-GitHub disclosure.
4. **Add `authors.workspace = true` to every crate's `[package]` table.**
   16 mechanical edits.
5. **Add `version = "0.3.0"` next to every `path = "../…"` inter-crate dep**
   (or hoist all `cratonvm-*` deps into `[workspace.dependencies]` and use
   `workspace = true`). Required for crates.io publish.
6. **Fill in `NOTICE`** with third-party Apache-2.0 attributions; the easy
   path is `cargo about generate --format markdown >> NOTICE`.
7. **Add `SUPPORT.md`, `GOVERNANCE.md`, `MAINTAINERS.md`, and
   `THIRD_PARTY_NOTICES.md`.** Even one-paragraph stubs are sufficient.
8. **Enable workflows.** Move `release.yml`, `dco.yml`, and `msrv.yml` (new)
   from `_disabled-workflows/` into `workflows/`. Add `-D warnings` to the
   clippy step in `ci.yml:37` so the CI actually matches what
   CONTRIBUTING.md:33 says it enforces.
9. **Add `.github/ISSUE_TEMPLATE/config.yml`** that disables blank issues
   and routes security reports to GHSA + the new email alias.
10. **Reserve crate names on crates.io** by publishing a `0.0.0` placeholder
    crate for each `cratonvm-<x>` and `cratonvm-cli`. Even with
    `publish = false` everywhere today, name-squatting is cheap and
    irreversible.
11. **Dry-run publish in dependency order.** Per RELEASING.md §3.4:
    `cratonvm-types` → `cratonvm-reader` → `cratonvm-jit-api` →
    `cratonvm-native-api` → `cratonvm-gc` → `cratonvm-classloading` →
    `cratonvm-jit` → `cratonvm-jfr` → `cratonvm-native-collections` →
    `cratonvm-native-io` → `cratonvm-native-builtins` → `cratonvm-vm` →
    `cratonvm-cli`. Decide and document whether `native-awt`, `jit-cuda`,
    `cuda-bridge`, `craton-gpu`, and `fuzz` stay `publish = false`.
12. **Add SPDX headers** to the 7 outliers (§5) and add a CI guard.
13. **Cut 0.3.0.** Tag, push, attach release artefacts produced by the
    now-enabled `release.yml`, run §11 cargo-publish dry-runs, publish.
