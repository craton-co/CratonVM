# CratonVM — Open-Source / crates.io Release-Readiness Audit

- **Module:** oss-readiness
- **Date:** 2026-06-10
- **Auditor:** Fable (release-readiness review)
- **Scope:** public GitHub release + crates.io publish, Apache-2.0, owner = Craton Software Company (craton.com.ar)
- **Method:** read-only file/metadata inspection + `git ls-files` / `git check-ignore`. No `cargo build`/`test` run.

Overall: **Not release-ready yet.** The foundations are unusually strong (exact Apache-2.0 LICENSE,
SPDX headers on ~all source, complete per-crate metadata + READMEs, versioned inter-crate deps,
docs.rs-safe build scripts, rich community-health docs). But there are a handful of **must-fix
blockers** before either a public push or a crates.io publish: committed TLS private keys that will
trip secret scanners and ship in a crate; missing third-party attribution (verbatim BouncyCastle
ports + a vendored LGPL Hibernate file) which is both an IP and an Apache-NOTICE-semantics problem;
CI that is entirely dormant (no `.github/workflows/`) behind a broken README badge; an over-broad
default publish gate that would push immature GPU/AWT crates; and a release runbook (RELEASING.md)
that contradicts the actual workspace config about publishing.

---

## 1. Licensing

### What's correct
- **LICENSE** is the exact, complete Apache-2.0 text (190 lines, full appendix) with the copyright
  line filled in: `Copyright 2024-2026 Craton Software Company. All rights reserved.` Good.
- **SPDX headers** are present on essentially all source: of 440 non-test production `.rs` files,
  only **one** lacks a header — `gc/src/shadow_stack.rs`. Headers use
  `// SPDX-License-Identifier: Apache-2.0` + `// Copyright 2024-2026 Craton Software Company`.
  This means the common "should files have headers?" recommendation is **already satisfied** — just
  patch the one straggler.
- **AUTHORS / MAINTAINERS / NOTICE / CODEOWNERS** are internally consistent: all attribute ownership
  to "Craton Software Company" and the `@craton-co/cratonvm-maintainers` GitHub team. NOTICE matches
  Apache-2.0 NOTICE form (short, copyright + license pointer).
- **Dependency licenses (Cargo.lock, 354 packages): no copyleft blockers.** Spot-checked the
  license-sensitive transitive crates — `ring` 0.17 (ISC/MIT/OpenSSL-legacy aggregate), `openssl`/
  `openssl-sys` (Apache-2.0 crate; links system OpenSSL 3.x = Apache-2.0), `libsqlite3-sys` (SQLite =
  public domain), `fontdue` (MIT/Apache/Zlib), `unicode-ident` (Unicode-DFS, permissive), `cudarc`,
  `mimalloc`, `libffi`, `p12`, `schannel`, `security-framework` — **all permissive**. No GPL/AGPL
  crate names found in the lock. The `ring`/OpenSSL aggregate licenses are permissive but warrant a
  one-line acknowledgment in a third-party notice (see below), not a code change.

### Blocker — verbatim BouncyCastle ports lack attribution and are mislabeled sole-Craton-copyright
`native-builtins/src/` contains files that are, by their own docstrings, **verbatim / faithful ports
of BouncyCastle Java code and precomputed tables**:
- `bc_newhope_tables.rs` — header literally reads *"Auto-extracted verbatim from BouncyCastle
  Precomp.java / NTT.java (Apache-2.0)."*
- `bc_newhope.rs` — *"a verbatim transcription of NTT/Reduce/Poly (BouncyCastle, Apache-2.0)."*
- `bc_aes.rs` — *"Tables and round code are mechanically transcribed from AESEngine.java
  (BouncyCastle, Apache-2.0)."*
- `bc_chacha.rs` — BouncyCastle ChaCha permutation kernels.

Two problems:
1. **License label is wrong / unverified.** These headers carry
   `SPDX-License-Identifier: Apache-2.0` **and** `Copyright 2024-2026 Craton Software Company` as if
   Craton authored them. BouncyCastle is actually distributed under the **Bouncy Castle License (an
   MIT variant)**, not Apache-2.0 — the docstrings asserting "BouncyCastle, Apache-2.0" appear to be
   incorrect about BC's license. Either way, claiming sole Craton copyright over mechanically-
   transcribed third-party code is an IP-provenance defect. MIT/BC code *can* be combined into an
   Apache-2.0 project, but the original copyright + permission notice must be preserved on the
   derived files, and Craton cannot assert exclusive copyright over the transcribed portions.
2. **No attribution surface.** NOTICE has no third-party section and there is no
   `THIRD-PARTY-NOTICES` / `licenses/` tree, so the BC origin is invisible to downstream users.

**Fix:** confirm BouncyCastle's actual license text, add the BC copyright + permission notice to each
`bc_*.rs` file's header (dual SPDX, e.g. `MIT AND Apache-2.0`, with a `// Portions derived from
BouncyCastle, (c) The Legion of the Bouncy Castle Inc.` line), and add a `THIRD-PARTY-NOTICES.md`
(or a `licenses/` dir) enumerating BC + the permissive native deps. This file ships inside the
`cratonvm-native-builtins` crate, so the attribution travels with the published artifact.

### Blocker — vendored LGPL Hibernate file at repo root
`../../../../apps/META-INF/services/jakarta.persistence.spi.PersistenceProvider` (tracked, at the repo root) is a
**Hibernate file carrying an LGPL v2.1-or-later header** (`License: GNU Lesser General Public License
(LGPL), version 2.1 or later. See the lgpl.txt file...`). It even references a `lgpl.txt` that does
not exist in the tree. Shipping an LGPL-headered third-party file at the root of an Apache-2.0 repo
is a provenance/compatibility flag. It looks like a stray test/runtime fixture rather than
intentional project content.

**Fix:** determine why it's there (Hibernate persistence-provider discovery during a test?). If it's
a fixture, move it under a clearly-scoped test path and/or `.gitignore` it; if it's genuinely
required, isolate it with its own LICENSE pointer. Do not leave an LGPL file unattributed at the
project root.

### Recommendation — add a third-party notice + NOTICE update
Add `THIRD-PARTY-NOTICES.md` listing: BouncyCastle (ports), `ring`/OpenSSL (aggregate licenses), and
the broader permissive-crate set. Optionally reference it from NOTICE. This is the single cleanest way
to satisfy Apache-2.0 §4(d) NOTICE-propagation semantics for the bundled third-party material.

---

## 2. Cargo metadata (per crate)

### What's correct
- Workspace version is **0.3.0** uniformly (`[workspace.package] version = "0.3.0"`); every member
  inherits via `version.workspace = true`.
- Every workspace member declares (directly or via `*.workspace = true`): `description`, `license`,
  `repository`, `homepage`, `readme`, `keywords`, `categories`, `edition`, `rust-version`. Several
  crates supply crate-specific `keywords`/`categories` (reader, types, cuda-bridge, craton-gpu) —
  accurate and useful.
- `repository`/`homepage` = `https://github.com/craton-co/cratonvm` consistently across all crates.
- **All inter-crate path deps in publishable crates declare a `version`** (`{ path = "...",
  version = "0.3.0" }`). Verified for every crate; the only `path`-without-`version` hits are a
  `[dev-dependencies]` entry in `native-collections` (dev-deps are exempt) and the multi-line
  `[dependencies.cuda-bridge]` block in `vm-cli` (the `version = "0.3.0"` is on its own line in the
  block). So the common "crates.io rejects bare path deps" problem is **already handled**.
- **Every crate's declared `readme = "README.md"` file actually exists** (all 17 checked). Good for
  crates.io rendering.
- **Package `exclude` lists are present** where they matter: `classloading`, `vm`, `vm-cli` exclude
  `tests/fixtures/**`, `tests/resources/**`, `**/*.class`, etc., so published crates don't drag
  opaque `.class` bytecode / multi-MB fixtures. Good.
- `fuzz` correctly sets `publish = false` and is intentionally not a workspace lint inheritor.
- `vm-cli` thoughtfully gates the `java[.exe]` second binary behind `java-bin-alias` (off by default)
  so `cargo install cratonvm-cli` won't shadow the system JDK launcher — a real crates.io safety win.

### Blocker — default publish gate is wide open; immature crates would publish
The root `Cargo.toml` comment (lines 6-9) states plainly: *"members do NOT inherit a publish gate...
so every library crate is publishable to crates.io. The only crate withheld is `fuzz`."* That means
`cratonvm-cuda-bridge`, `cratonvm-jit-cuda`, `cratonvm-gpu` (package name of `craton-gpu`), and
`cratonvm-native-awt` are **all publishable by default** — exactly the early-maturity / platform-glue
crates that should arguably stay private at first release. A `cargo publish` of the workspace (or of
`cratonvm-vm` which transitively requires their versioned metadata) would push them.

**Fix:** decide a per-crate publish policy and add `publish = false` to the `[package]` table of any
crate that should not ship at 0.3.0 (candidates: `cuda-bridge`, `jit-cuda`, `craton-gpu`,
`native-awt`). This also resolves the RELEASING.md contradiction below.

### Blocker — RELEASING.md contradicts the actual workspace config
`RELEASING.md` §3 says *"The workspace currently sets `publish = false` at `[workspace.package]`, so
nothing is pushed to crates.io"* and instructs the operator to opt **in** per crate with
`publish = true`. The actual `[workspace.package]` sets **no** `publish` key (confirmed by reading
Cargo.toml), and the root comment documents the opposite default. An operator following RELEASING.md
would believe crates are gated-off and add `publish = true` selectively — while in reality every
non-`fuzz` crate is already publishable. This is a footgun that could push unintended crates.

**Fix:** rewrite RELEASING.md §3 to match reality (default = publishable; add `publish = false` to
withhold), or change the config to actually gate by default — and make the two agree.

### Warning — `homepage` should likely be the company site, and a domain inconsistency exists
Per the owner brief the company domain is **craton.com.ar**, but the entire repo uses **`craton.co`**:
emails `hello@craton.co` / `security@craton.co`, org `craton-co`, repo `github.com/craton-co/cratonvm`.
`homepage` is currently the GitHub repo (= `repository`), which is redundant. Recommend pointing
`homepage` at the company site once the canonical domain is confirmed. **The auditor cannot tell
whether `craton.co` or `craton.com.ar` is correct** — this needs an owner decision (see §4).

### Warning — `craton-gpu/Cargo.toml` omits `[lints] workspace = true`
Unlike every other member, `craton-gpu` (package `cratonvm-gpu`) has no `[lints] workspace = true`
and no `[dependencies]`/`[dev-dependencies]` tables (it is a pure build-time annotation crate). The
missing lint inheritance is cosmetic but inconsistent; add it for uniformity.

### TODO (cannot verify offline) — crate-name availability on crates.io
All 17 publishable names use the `cratonvm-*` / `cratonvm` prefix (and `cratonvm-gpu` for the
`craton-gpu` dir). Availability of each name on crates.io cannot be checked in this offline audit.
**Action before publish:** `cargo search`/registry-check each name; reserve them early.

---

## 3. Repo hygiene

### What's correct
- `.gitignore` is comprehensive for `target/`, `target-gpu/`, `applogs/`, `felix-cache/`, `scratch/`,
  `.bench-cache/`, `dist/*.exe|*.dll`, `*.exe`, `**/*.jar`, `**/*.tar.gz`, `.idea/`, `.claude/`,
  `apps/`, `uk/` (untracked — verified 0 tracked files in each), `hs_err_pid*.log`, etc.
- **No jars / exes / zips / dlls / tarballs are tracked** (verified via `git ls-files` filter). The
  largest tracked files are all legitimate source (`vm/src/vm.rs` ~2.2 MB, `native-builtins`/`jit`
  source). No oversized binaries ship.
- `apps/`, `uk/` are fully untracked. `.idea/`, `applogs/`, `felix-cache/`, `scratch/` = 0 tracked.
- **No hardcoded passwords/keys/tokens in tracked scripts or configs.** The `password=...` /
  `POSTGRES_PASSWORD` / `spring.datasource.password=...` hits all live under `.claude/worktrees/**`,
  which is gitignored — none of those files (`run-sportme.sh`, `docker-compose.yml`) are git-tracked.

### Blocker (security-scanner + crate-content) — committed TLS private keys
`native-builtins/src/t27_certs/` contains **four `-----BEGIN PRIVATE KEY-----` files** (`server.key`,
`server1.key`, `server2.key`, `client.key`) plus matching certs. They are confirmed **self-signed
test fixtures** (`CN=RustJVM T2.7 Test CA`) and are consumed only via `include_str!` under
`#[cfg(test)]` in `t27_tls.rs` (never in release binaries — the code comments are explicit and
correct about fail-closed release behavior). **But** two release problems remain:
1. **GitHub secret scanning / push protection** (and tools like TruffleHog/Gitleaks) flag committed
   `PRIVATE KEY` PEM blocks; this can **block or alarm the public push** even though the keys are
   harmless test material.
2. `native-builtins/Cargo.toml` has **no `exclude`**, so `t27_certs/*.key` would ship as source in
   the published `cratonvm-native-builtins` crate.

**Fix:** either (a) generate the test keys at build/test time instead of committing them, or
(b) keep them but add `exclude = ["src/t27_certs/**"]` to `native-builtins/Cargo.toml` and add a
`SECURITY.md`/scanner allowlist note (e.g. `.gitleaks.toml` / secret-scanning allowlist) documenting
that they are throwaway self-signed test keys. Excluding them from the package is the minimum;
re-generating is cleaner.

### Warning — committed run logs and a suite-results directory will ship
- **42 tracked `*.log` files (~110 KB total)**: e.g. `bench/keycloak26/last-run.stdout.log`,
  `bench/wave2-3/last-run-bytebuddy.stderr.log`, `bench/wave2-4/jacoco-*.log`, and a
  `test-infra/suite-results/apps-all-20260605-235553/` run directory (20 files incl. per-app
  `*-cratonvm.log` / `*-hotspot.log`, `results.tsv`, `stage.log`) plus `cpu-rerun-20260605-235403.log`.
  These were committed **before** the relevant ignore rules existed (gitignore doesn't untrack), and
  `.gitignore` has **no generic root `*.log` rule** — only specific paths (`server.log`, `applogs/`,
  `err_all.log`, `hs_err_pid*.log`, `test-infra/suite-results/*.tsv`, `_cm-*.log`). So these stale
  per-run logs will appear in the public repo.
- **Fix:** `git rm --cached` the tracked logs + the `apps-all-*` results dir, and add a generic
  `*.log` (plus `test-infra/suite-results/apps-all-*/` and `test-infra/suite-results/*-rerun-*.log`)
  to `.gitignore`. Note `build_cpu_rerun.log` and `build-cpu*.bat` are currently **untracked**
  (uncommitted) — a generic `*.log`/`build-cpu*.bat` ignore would prevent accidental `git add`.

### Warning — personal machine paths (`C:/Users/Victor`) committed
Three tracked files embed a developer's home path:
- `scripts/build-devverify.bat:7-8` — `C:\Users\Victor\.rustup\...\hmrustc.exe` / `hmcargo.exe`
  (note: this also references **non-standard `hm`-prefixed rustc/cargo wrappers**, which won't exist
  on a contributor's machine — the script is effectively single-developer-only).
- `docs/gaps/gap-nio-basicfileattributes-isdirectory.md:70` and
  `docs/gaps/gap-jit-dispatch-exception-wrapping.md:36` — `M2="C:/Users/Victor/.m2/repository"`.
- `craton-gpu/build.rs` also hardcodes a `C:/craton/craton-gpu-java/` default source path (it falls
  back gracefully, so not a build blocker, but it's a machine-specific assumption to parameterize).

**Fix:** parameterize the `.bat` paths (use `%USERPROFILE%` / PATH-resolved `cargo`/`rustc`), and
genericize the doc-file `M2` examples (`$HOME/.m2/...` or `${M2_REPO}`). These are not secrets but
they leak a contributor identity and are non-portable.

### Note — large `.class` fixture footprint (contained)
539 `.class` files are tracked, but the bulk sit under `vm/tests/resources/**` (239+174) and
`bench/**` (92), and the package `exclude` lists drop `**/*.class` from the published `vm`/`vm-cli`/
`classloading` crates. `bench/`, `dist/examples/`, `tools/`, `bench-tornado/`, `test-tern/` are not
workspace members, so they bloat the **git repo** (~1.7 MB for `bench/`) but not any **published
crate**. Acceptable for a research-JVM repo; flag only if repo size is a concern.

---

## 4. GitHub readiness

### What's correct (strong)
- Full community-health set present and substantive: `README.md`, `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, `SECURITY.md`, `GOVERNANCE.md`, `MAINTAINERS.md`, `SUPPORT.md`, `ROADMAP.md`,
  `CHANGELOG.md`, `ARCHITECTURE.md`, `RELEASING.md`, `AUTHORS`, `NOTICE`.
- `.github/`: `CODEOWNERS` (per-crate ownership), `ISSUE_TEMPLATE/` (bug + feature + config),
  `pull_request_template.md`, `dependabot.yml` (cargo + github-actions, weekly), `FUNDING.yml`.
- `README.md` "Status & Known Limitations" is **commendably honest** (not certified, don't run
  untrusted code, crypto not production-ready, known JIT crash) — exactly right for a research JVM.
- DCO enforcement is designed-in (a `dco.yml` workflow + `Signed-off-by` policy in CONTRIBUTING).

### Blocker — CI is entirely dormant; the README CI badge is broken
GitHub Actions only executes workflows in **`.github/workflows/`**, which **does not exist**. All 13
workflow YAMLs live in `.github/.wf/` (a non-standard dir GitHub ignores) and two stale copies in
`.github/_disabled-workflows/`. Consequences on public release:
- **No CI runs at all** — no build/clippy/test gate on PRs, no release/bench/smoke automation.
- The README badge `[![CI](.../actions/workflows/ci.yml/badge.svg)]` points at a workflow path that
  doesn't exist → the badge renders permanently **grey/"no status"**.
- **Self-contradiction:** `.github/.wf/README.md` says in one paragraph *"Every workflow in this
  directory is active"* and in the next *"GitHub Actions only scans `.github/workflows/`, so anything
  under `.github/.wf/` is dormant."* Both can't be true; the second is correct.

**Fix:** move the intended workflows (at minimum `ci.yml`, `dco.yml`, `cuda-bridge.yml`,
`release.yml`) into `.github/workflows/`, fix the `.wf/README.md` wording, and verify the README
badge URL matches the activated `ci.yml`. Delete `_disabled-workflows/` or document why it exists.

### Warning — README references a non-existent `TRADEMARKS.md`
`README.md` §License links to `[TRADEMARKS.md](TRADEMARKS.md)` for "trademark attributions and
notices," but **no `TRADEMARKS.md` exists** in the tree. Either add the file (Java is an Oracle
trademark — a research JVM should carry the standard "Java is a trademark of Oracle" notice and an
"unofficial/uncertified" disclaimer) or remove the dead link.

### Warning — contact-domain inconsistency vs. owner brief (`craton.co` vs `craton.com.ar`)
`SECURITY.md` directs reports to `security@craton.co`; `MAINTAINERS.md` lists `hello@craton.co` /
`security@craton.co`; `FUNDING.yml` uses `github: craton-co`. The owner brief names the company site
as **craton.com.ar**. `SECURITY.md` does **not** reference `craton.com.ar`. This may be intentional
(`.co` for product, `.com.ar` for corporate) or a typo. **Owner must confirm** the canonical
domain(s); if `.com.ar` is correct, update SECURITY.md / MAINTAINERS.md / homepage accordingly.

---

## 5. crates.io readiness

### What's correct
- **Every publishable crate has its own `README.md`** (verified all 17), so crates.io pages render.
- **docs.rs is buildable** — this was the highest crates.io risk and it's handled well:
  - All three `build.rs` scripts (`craton-gpu`, `jit-cuda`, `vm`) **invoke `javac` only opportunistically
    and degrade to `cargo:warning=` when it's absent — they never fail the build.** docs.rs has no
    `javac`, so they'll warn-and-continue. Good.
  - **No CUDA toolkit is required at doc/build time.** `cuda-bridge` has **no `build.rs`**, and its
    `cudarc` dep is `optional` behind a non-default `cuda` feature — a plain `cargo doc` compiles the
    stub backend only. No `#[link]`/`rustc-link-lib` to a CUDA lib in default builds.
  - The MSVC/AWT platform deps (`windows`, `x11rb`, `objc2*`) are `[target.'cfg(...)']`-gated, so
    docs.rs (Linux) only pulls the Linux set.
- **MSRV claim `rust-version = "1.77"` is plausible** for this dependency set (edition 2021, no
  bleeding-edge std features observed in the manifests; `hashbrown 0.14`/`raw_entry_mut`,
  `bitfield-struct 0.9`, `clap 4`, etc. all support 1.77-era toolchains). Recommend a CI job pinned
  to 1.77 to *prove* it once workflows are activated (can't be verified offline here).

### Recommendation — add a publish-order dry-run gate before first publish
RELEASING.md already documents the correct leaf-first publish order and `--dry-run` discipline. Once
the publish policy (§2 blockers) is settled, run `cargo publish --dry-run -p <crate>` for each
publishable crate in that order to catch any remaining bare-path-dep / missing-field issues that only
surface at package time. (Cannot be executed in this read-only audit.)

---

## Summary of required actions (priority order)

**Blockers (fix before any public push or publish):**
1. Remove/exclude committed TLS private keys (`native-builtins/src/t27_certs/*.key`) — secret-scanner
   + crate-content risk.
2. Attribute the verbatim BouncyCastle ports (`bc_aes.rs`, `bc_chacha.rs`, `bc_newhope*.rs`) and add a
   third-party notice; correct the sole-Craton-copyright / Apache-2.0 mislabel.
3. Resolve the LGPL Hibernate file at repo root (`../../../../apps/META-INF/services/...PersistenceProvider`).
4. Activate CI: move workflows into `.github/workflows/`; fix the broken README CI badge; fix the
   self-contradictory `.wf/README.md`.
5. Set a per-crate publish policy (`publish = false` on `cuda-bridge`/`jit-cuda`/`craton-gpu`/
   `native-awt`) — default is currently "publish everything."
6. Fix RELEASING.md §3 to match the real workspace publish config.

**Warnings:**
- `git rm --cached` the 42 tracked logs + `apps-all-*` results dir; add generic `*.log` to .gitignore.
- Genericize personal paths (`C:/Users/Victor`) in `scripts/build-devverify.bat` + two `docs/gaps/*`.
- Add the missing `TRADEMARKS.md` (or drop the README link); include the Oracle/Java trademark notice.
- Confirm canonical domain (`craton.co` vs `craton.com.ar`) and align SECURITY.md/MAINTAINERS/homepage.
- Add `[lints] workspace = true` to `craton-gpu/Cargo.toml`; add SPDX header to `gc/src/shadow_stack.rs`.

**Recommendations:**
- Add `THIRD-PARTY-NOTICES.md` (BouncyCastle + ring/OpenSSL + permissive deps); reference from NOTICE.
- Consider adding `deny.toml` + a `cargo-deny` CI job (no `deny.toml` exists today) to keep the
  license/advisory posture honest as deps churn.
- Point `homepage` at the company site (once domain confirmed) instead of duplicating `repository`.
- TODO: verify all 17 `cratonvm-*` crate names are available on crates.io and reserve them.
- Run leaf-first `cargo publish --dry-run` across the workspace before the real publish.
- Add a 1.77-pinned MSRV CI job to substantiate the `rust-version` claim.
