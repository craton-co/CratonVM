# CI evidence: what each workflow proves, and how to reproduce it

This document is the reader's guide to the CI additions made for the
"Reproducibility" and "Cross-platform CI" lanes. It answers four questions:

1. What does each workflow actually prove — and, just as important, what does a
   green run *not* prove?
2. How do I run every leg locally, on Windows and on Linux?
3. How do I read the evidence manifest?
4. What has to be true before a non-blocking leg becomes a gate?

Everything described here is **additive**. No existing job was made stricter; in
particular the Clippy step in [`ci.yml`](../../.github/workflows/ci.yml) is
untouched, and no lint table in the workspace manifest was modified.

---

## 1. The workflow inventory

| Workflow | Blocking? | Trigger | Proves |
| --- | --- | --- | --- |
| [`ci.yml`](../../.github/workflows/ci.yml) | **Yes** (pre-existing) | push/PR | default configuration builds, is formatted, passes configured Clippy and the workspace tests, on Linux + Windows |
| [`feature-matrix.yml`](../../.github/workflows/feature-matrix.yml) | **Yes** | push/PR to `main`/`dev` | every declared feature and a curated set of combinations still **compiles**, test targets included |
| [`cross-platform.yml`](../../.github/workflows/cross-platform.yml) | No — every leg `continue-on-error` | push/PR to `main`/`dev` | macOS x86-64/arm64, Linux AArch64 (cross + native), and the Windows C ABI |
| [`strict-lints.yml`](../../.github/workflows/strict-lints.yml) | No | push/PR to `main`/`dev` | size of the `unwrap`/`expect`/raw-pointer-safety debt in the five safety-critical crates |
| [`sanitizers.yml`](../../.github/workflows/sanitizers.yml) | No | nightly cron + dispatch | ASan/LSan, UBSan/UB-checks, TSan over real execution |
| [`miri.yml`](../../.github/workflows/miri.yml) | No | push/PR to `main`/`dev` | UB in the parser/metadata layer under an interpreting checker |
| [`supply-chain.yml`](../../.github/workflows/supply-chain.yml) | No | push/PR + nightly cron | RUSTSEC advisories, licences, bans, sources, duplicates, SBOM |
| [`coverage.yml`](../../.github/workflows/coverage.yml) | Yes (pre-existing) | push/PR | LCOV report generation |

`ci.yml` keeps its own blocking Miri job (`cratonvm-types::heap_types`) and its
own hand-written `synthetic-jdk` / `experimental-features` feature gates. Those
were not removed: `feature-matrix.yml` compiles *more* configurations but does
not *run* the feature-gated test suites, and `miri.yml` widens scope at the cost
of being non-blocking. Deleting either of the `ci.yml` jobs in favour of the new
workflows would lose signal.

---

## 2. What each workflow proves — and what it does not

### 2.1 `feature-matrix.yml` — configuration rot

**Proves.** Every non-default feature declared by any workspace member, plus a
curated pairwise/triple set, plus `--no-default-features`, compiles under
`cargo check --workspace --all-targets`.

**Does not prove.** That any of those configurations *behaves* correctly.
Behaviour of the non-default configurations is `ci.yml`'s job — it runs the
`synthetic-jdk` and `experimental-*` test suites.

**Why it exists.** `cargo build --workspace` compiles exactly one configuration.
`cargo build --features synthetic-jdk` has been found broken on `dev`
with 22 name-resolution errors in production code and 327 more errors' worth of
API drift behind them, in a test module gating 1,522 tests that no compiler had
looked at for months. Nothing was red, because nothing built that configuration.

**Why the matrix is generated, not written down.** `ci.yml`'s feature jobs are a
hand-maintained list; a feature added tomorrow does not join it. The `enumerate`
job derives the list from `cargo metadata`, so a newly declared feature is
compiled from the moment it exists.

**Why `--all-targets`.** Lib+bin scope would have caught 22 of those 349 errors.
The rest lived in test modules, examples and benches.

**Size and cost.** The workspace declares **52** non-default features across 13
members. Twelve are `gpu`/`cuda` and are excluded, so the matrix is ~53 legs:
40 single-feature, 2 baselines (`default` and `--no-default-features`), and 11
curated combinations. That is the price of the guarantee. `paths-ignore` skips
docs-only changes, `max-parallel` is capped at 6 so this can never queue ahead
of the blocking `ci.yml` jobs, and the workflow header documents the two dials
for reducing it further — including the trap that collapsing a feature to its
*leaf* owner instead of its *root* forwarder silently deletes coverage.

**Deliberate exclusion.** Feature names matching `gpu` or `cuda` are routed to
[`cuda-bridge.yml`](../../.github/workflows/cuda-bridge.yml) and
[`gpu-selfhosted.yml`](../../.github/workflows/gpu-selfhosted.yml), which have
the hardware. Everything else is in.

### 2.2 `cross-platform.yml` — portability

**Proves.** That the workspace compiles (`cargo check --workspace
--all-targets`) on macOS x86-64, macOS arm64, Linux AArch64 cross-compiled, and
Linux AArch64 natively; and that `libcratonvm.dll`'s export table agrees with
the committed `cratonvm.h` on Windows.

**Does not prove.** Any runtime behaviour on those platforms. These are compile
legs. The one exception is the native AArch64 leg, which additionally *runs*
`cargo test -p cratonvm-reader -p cratonvm-types --lib` — the two crates with no
architecture-specific code.

**Why every leg starts advisory.** None of these platforms has ever been
compiled in CI, so they start red by construction. Advisory converts "unknown"
into "a measured error list" on day one; blocking would stop all merges for
defects that predate the workflow. Promote **per leg**, never by deletion.

**What to expect when reading the first red runs.** The JIT emits x86-64
machine code, so the AArch64 legs will fail in `cratonvm-jit` unless the
`#[cfg(target_arch = "x86_64")]` guards are already complete — that is exactly
the fact the leg is there to establish. The narrow per-crate check steps exist
so the answer is legible even when the workspace check is red.

### 2.3 `strict-lints.yml` — safety-lint debt

**Proves.** How many findings the strict set (`-D warnings`, plus
`not_unsafe_ptr_arg_deref`, `mut_from_ref`, `unwrap_used`, `expect_used`)
produces in `cratonvm-reader`, `cratonvm-types`, `cratonvm-jit-api`,
`cratonvm-jit` and `cratonvm-gc`.

**Does not prove.** Anything about the rest of the workspace, and nothing at
all if the `-D` flags fail to take effect. The root manifest sets all four of
those lints to `allow`; the command-line `-D` overrides it because cargo passes
manifest lints as flags and rustc applies the last one. **A summary showing
zero for every lint must be checked against the log** before being read as
"clean" — the job prints per-lint counts for exactly this reason.

**Promotion.** One lint, one crate at a time: drive it to zero, then move that
lint out of the workspace `allow` table into that crate's own `[lints.clippy]`
as `deny`.

### 2.4 `sanitizers.yml` — runtime memory and concurrency errors

**Proves.** That the interpreter, class reader, metadata layer, GC bookkeeping
and native/FFI surface are free of the errors ASan/LSan/TSan can observe, on the
code paths the tests exercise.

**Does not prove — and this is the important one.** ASan works by having LLVM
rewrite every load and store at compile time. The JIT emits x86-64 machine code
at run time into a W^X mapping; rustc never sees those bytes, so **no
JIT-compiled execution path is instrumented at all**. A green ASan run says
nothing whatsoever about JIT-compiled code. The full statement of this limit,
including the LSan suppressions it forces, is in
[`scripts/evidence/asan-suppressions.txt`](../../scripts/evidence/asan-suppressions.txt)
— read it before quoting a green run.

To narrow the blind spot, run the sanitizer legs with the JIT disabled so the
interpreter executes everything; the interpreter *is* instrumented.

**TSan caveat.** TSan is scoped to `cratonvm-types`, `cratonvm-classloading`,
`cratonvm-gc` and `cratonvm-vm`. It cannot see happens-before edges established
by safepoint transitions or JIT-emitted barriers, so reports whose two stacks
straddle one of those are very likely false positives. Check against
`types/src/lock_order.rs` before filing.

**UBSan caveat.** rustc's `-Zsanitizer=` list is target-dependent and does not
universally accept `undefined` — that is a C/C++ facility, whereas rustc's own
UB story is `-Zub-checks` plus debug assertions. The job runs both and records
both outcomes rather than pretending one is the answer.

### 2.5 `miri.yml` — UB in the pure crates

**Proves.** That the `unsafe` blocks in `cratonvm-reader` and `cratonvm-types`
hold up under a UB-checking interpreter with the aliasing model enabled.

**Does not prove.** Anything about executable memory (Miri has no CPU and cannot
execute generated code), FFI (unsupported operation), GC root scanning (walks
real stacks and rewrites real addresses), or any thread that makes a platform
call. `cratonvm-native-*`, `cratonvm-vm` and `libcratonvm` are out of scope by
construction, not by choice.

**Triage rule.** "Undefined Behavior" findings are real defects. "unsupported
operation" stops are Miri limitations. The job's summary step separates the two
counts because conflating them is how a Miri leg gets written off as noise.

### 2.6 `supply-chain.yml` + `deny.toml` — provenance

**Proves.** Whether any dependency carries a RUSTSEC advisory; whether every
transitive licence is compatible with redistributing an Apache-2.0 binary;
whether a git or alternate-registry dependency has appeared; which duplicate
version families exist; and what is in the artifact (CycloneDX SBOM).

**Does not prove.** That the shipped binary matches the SBOM — that requires a
reproducible-build attestation, which is a later step. The SBOM job is a
producer, not a gate, and stays advisory permanently.

**Why `deny.toml` starts permissive.** It has never been run against this graph.
`bans.multiple-versions` starts at `allow` because the root `Cargo.toml`
documents three knowingly-accepted duplicate families (hashbrown, getrandom,
windows-sys), each a hard API break that cannot be unified with a `[patch]`.
`sources` is strict from the start, because "crates.io and nothing else" is a
provenance question with an expected answer, not a taste question. Every TODO in
that file names the evidence needed to tighten it.

---

## 3. Reproducing every leg locally

All commands are run from the repository root. Where Windows and Linux differ,
both are given.

### 3.1 The evidence manifest

Linux/macOS:

```sh
bash scripts/evidence/collect.sh
# or to a different directory:
bash scripts/evidence/collect.sh out/manifest
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\evidence\collect.ps1
powershell -ExecutionPolicy Bypass -File scripts\evidence\collect.ps1 -OutDir out\manifest
```

Both write the same file names with the same section banners, so a Linux
manifest and a Windows manifest diff cleanly against each other. Neither needs a
build; both are seconds. Both tolerate missing optional tools (a machine with no
`javac` gets a recorded `not available` line, not a failure).

### 3.2 Feature matrix

The workflow's `enumerate` job is reproducible on its own:

```sh
cargo metadata --format-version 1 --no-deps > metadata.json
jq -r '.packages[] | . as $p | ($p.features // {} | keys[]) | select(. != "default") | "\($p.name)/\(.)"' metadata.json | sort -u
```

On Windows, `scripts\evidence\collect.ps1` writes the same inventory to
`evidence\cargo-features.txt` without needing `jq`.

Then any single leg:

```sh
cargo check --workspace --all-targets
cargo check --workspace --all-targets --no-default-features
cargo check --workspace --all-targets --features cratonvm-vm/synthetic-jdk
cargo check --workspace --all-targets --features cratonvm-vm/synthetic-jdk,cratonvm-vm/zgc
```

Feature names are written `package/feature` rather than bare, because four
crates declare a feature called `synthetic-jdk` and a bare `--features
synthetic-jdk` enables all of them at once — a different configuration from any
one of them alone.

### 3.3 Cross-platform

```sh
# Linux AArch64 cross-compile
rustup target add aarch64-unknown-linux-gnu
sudo apt-get install -y gcc-aarch64-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  cargo check --workspace --all-targets --target aarch64-unknown-linux-gnu
```

macOS legs are a plain `cargo check --workspace --all-targets` on the host.

Windows C-ABI smoke test (from a normal shell; the batch file finds MSVC):

```bat
cargo build --release -p libcratonvm
call scripts\find-vcvars.bat
call "%VCVARS64%"
dumpbin /EXPORTS target\release\libcratonvm.dll
cl /nologo /I libcratonvm\include libcratonvm\examples\embed_smoke.c /Fe:embed_smoke.exe /link target\release\libcratonvm.dll.lib
```

The link step is the test; the example is not executed, because running it would
boot a VM and need a JDK image, which is a runtime test rather than an ABI test.

### 3.4 Strict lints

Identical on both platforms:

```sh
cargo clippy -p cratonvm-reader -p cratonvm-types -p cratonvm-jit-api -p cratonvm-jit -p cratonvm-gc --all-targets -- -D warnings -D clippy::not_unsafe_ptr_arg_deref -D clippy::mut_from_ref -D clippy::unwrap_used -D clippy::expect_used
```

### 3.5 Sanitizers

**Linux only.** ASan/TSan on Windows would need a different runtime and are not
part of this lane.

```sh
rustup +nightly component add rust-src

# ASan
RUSTFLAGS="-Zsanitizer=address -C force-frame-pointers=yes" \
ASAN_OPTIONS="detect_stack_use_after_return=1:detect_leaks=1:print_stacktrace=1" \
LSAN_OPTIONS="suppressions=scripts/evidence/asan-suppressions.txt" \
  cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu --workspace

# UB checks (always available, no build-std required)
RUSTFLAGS="-Zub-checks=yes -C debug-assertions=on" cargo +nightly test --workspace

# TSan, concurrency crates only
RUSTFLAGS="-Zsanitizer=thread -C force-frame-pointers=yes" \
TSAN_OPTIONS="halt_on_error=0:history_size=7" RUST_TEST_THREADS=1 \
  cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu \
  -p cratonvm-types -p cratonvm-classloading -p cratonvm-gc -p cratonvm-vm --lib
```

`--target` is mandatory with `-Zbuild-std`: without it, `RUSTFLAGS` is applied
to build scripts and proc macros too, and those run on an uninstrumented host
runtime.

If a run fails inside mimalloc, narrow it with `--exclude cratonvm-cli` —
mimalloc and ASan both interpose `malloc` and must not be linked together. Do
not add a suppression for it.

### 3.6 Miri

Works on Windows and Linux, with the caveat recorded in `ci.yml` that some
`cratonvm-types` thread tests stop with an unsupported-operation error on
Windows:

```sh
rustup +nightly component add miri
MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks" \
  cargo +nightly miri test -p cratonvm-reader --lib
MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks" \
  cargo +nightly miri test -p cratonvm-types --lib
```

The blocking subset that `ci.yml` gates on:

```sh
cargo +nightly miri test -p cratonvm-types --lib heap_types
```

### 3.7 Supply chain

```sh
cargo install --locked cargo-audit cargo-deny cargo-cyclonedx
cargo audit
cargo deny check sources
cargo deny check licenses
cargo deny check bans
cargo deny check advisories
cargo tree --duplicates --workspace
cargo cyclonedx --format json --all
```

Identical on Windows (PowerShell), with `cargo install` unchanged.

---

## 4. Reading the evidence manifest

`scripts/evidence/collect.sh` and `collect.ps1` write six files.

### `source.txt` — which tree

Read it top to bottom, but the section that decides whether anything else
matters is **`worktree cleanliness`**:

* `CLEAN: tracked files match HEAD.` — the recorded commit reproduces the result.
* `DIRTY: ...` followed by a diffstat — it does not. Any number produced
  alongside a dirty manifest is provisional and must not be quoted as a
  measurement of that commit.

The rest: `HEAD` and `git describe` identify the tree; `git submodule status`
keeps its shape for the day a submodule is added (there is no `.gitmodules`
today); the last 30 commits give the trunk context.

The **subsystem history** section is the part that is specific to this project.
It carries the recent history of `jit/`, `gc/`, `vm/src/runtime`,
`types/src/value.rs` and `classloading/` separately, because a change in any of
those silently invalidates a previously recorded benchmark, GC audit or
differential verdict. When comparing two results, diff *this* section first: if
it is identical, a difference between the runs is environmental.

### `environment.txt` — which machine and toolchain

* `rustc -Vv` carries the commit hash and host triple — the two facts that make
  a codegen difference explainable.
* **`CPU features`** is load-bearing here specifically. `.cargo/config.toml`
  pins `-C target-feature=+sse4.2,+pclmulqdq` for x86-64; a host without them
  produces a binary that will not run, and a host with AVX-512 gets different
  auto-vectorisation. The Windows collector probes the individual features via
  `IsProcessorFeaturePresent` because Win32 has no `/proc/cpuinfo`.
* `javac -version` reporting "not available" explains a class of confusing test
  results: `vm/build.rs` compiles Java fixtures with `javac`, and without one
  the fixtures are skipped rather than failed.
* The environment-variable section records *values*, not just names.
  `CRATONVM_JIT=off` and `CRATONVM_JIT=on` are different runs, and a manifest
  that only said "CRATONVM_JIT was set" would be useless.

### `cargo-metadata.json`, `cargo-tree.txt`, `cargo-duplicates.txt`

The resolved dependency graph. `cargo-duplicates.txt` is prefixed with the three
accepted duplicate families from the root `Cargo.toml`; anything outside those
three is new and should be triaged rather than absorbed.

### `cargo-features.txt`

Every feature every workspace member declares, the default sets, and a flat
list of qualified `package/feature` names — the same list
`feature-matrix.yml` enumerates. Use it to check that a configuration you are
about to claim is tested is actually in the matrix.

---

## 5. Acceptance criteria per lane

### Lane: Reproducibility

| Criterion | Status |
| --- | --- |
| A result can be traced to a commit, a toolchain and a CPU | **Met** — `scripts/evidence/collect.sh` / `.ps1` |
| The manifest is producible identically on Windows and Linux | **Met** — twin scripts, identical file names and section banners |
| A dirty worktree is visible, not silent | **Met** — the `worktree cleanliness` section fails loudly in text |
| Dependency provenance is recorded | **Met** — `cargo-metadata.json` + SBOM job |
| Every configuration that exists is compiled by something | **Met** — `feature-matrix.yml`, generated from `cargo metadata` |
| The shipped binary is byte-reproducible from the manifest | **Not met** — needs a reproducible-build attestation; out of scope here |

### Lane: Cross-platform CI

| Criterion | Status |
| --- | --- |
| macOS x86-64 compiles | **Measured, advisory** — `cross-platform.yml` / `macos` |
| macOS arm64 compiles | **Measured, advisory** — same job, `macos-14` leg |
| Linux AArch64 compiles | **Measured, advisory** — cross and native legs |
| Windows C ABI matches the shipped header | **Measured, advisory** — `windows-abi-smoke` |
| Any non-x86-64 platform passes the test suite | **Not met** — only the two architecture-neutral crates are run, natively on AArch64 |

A leg is promoted from advisory to blocking by driving its error list to zero,
deleting its `continue-on-error: true`, and adding it to branch protection — in
that order, one leg at a time.

### Lane: Lints, sanitizers, supply chain

| Criterion | Status |
| --- | --- |
| The safety-lint debt has a number | **Met** — `strict-lints.yml` artifact |
| Runtime memory errors are checked on real execution | **Partly met** — ASan/TSan cover everything except JIT-generated code, which cannot be instrumented |
| UB is checked in the parser/metadata layer | **Met** — `ci.yml` (blocking, narrow) + `miri.yml` (advisory, wider) |
| Dependencies are checked for advisories and licences | **Met, advisory** — `supply-chain.yml` |
| An SBOM exists for what ships | **Met** — CycloneDX, per release artifact bundle |
| JIT-generated code is checked for memory errors | **Not met, and not achievable with these tools** — needs guard pages, hardware watchpoints, or the VM's own W^X and card-table assertions |

---

## 6. Housekeeping

**`evidence/` is not in `.gitignore` yet.** Both collectors write into
`evidence/` at the repository root, and that directory should never be
committed — it is a per-run artifact containing machine-specific paths and
toolchain versions. Add this to `.gitignore`:

```gitignore
# Per-run reproducibility manifests written by scripts/evidence/collect.{sh,ps1}
/evidence/
```

Until that lands, either pass an explicit output directory that is already
ignored (`bash scripts/evidence/collect.sh target/evidence`) or delete
`evidence/` after reading it. CI is unaffected — the workflows upload the
directory as an artifact and never commit it.

**Line endings.** `scripts/evidence/collect.sh` must reach a Linux runner with
LF endings. The repository is developed on Windows with `core.autocrlf=true`,
which normalises to LF on commit, so the default path is correct — but
`.gitattributes` already pins two other shell scripts with `text eol=lf` after
this exact problem bit once before. If a CRLF `collect.sh` ever reaches CI, the
fix is an `eol=lf` entry for `scripts/evidence/*.sh`, not a change to the
script.
