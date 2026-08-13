# CratonVM — Full Multi-Agent Review (2026-06-17)

**Scope:** every workspace crate + docs + scripts + OSS metadata.
**Method:** 66 parallel Opus agents (one+ per module; the two giant crates
`native-builtins` 322K LOC and `vm` 234K LOC were split into 9 and 7
sub-agents). Each agent ran risk-pattern sweeps (`unsafe`, `transmute`,
`unwrap`, `from_raw`, `as usize`, wire-length reads, …) plus targeted deep
reads, and returned structured findings. Headline findings were
source-verified by hand before publication.

> Source artifact for the raw per-agent JSON: workflow run `wf_5ee33a6f-111`
> (66/66 agents succeeded, 0 failures, ~6.8M agent tokens).

---

## Headline rollup

| Category | Count |
|---|---|
| Critical bugs | 4 |
| High bugs | 37 |
| Medium bugs | 49 |
| Low bugs | 43 |
| Security vulnerabilities (incl. above) | 19 |
| Stubs / unimplemented / fake-behavior | 47 |
| Performance items | 54 |

**Overall:** CratonVM is a remarkably mature, heavily-hardened JVM. The
parser (`reader`), interpreter, JIT core, GC core, and `classloading` are
in genuinely good shape (extensive `SAFETY` discipline, prior-audit-fix
comments, behavioral tests). The defects cluster into a small number of
**recurring structural patterns**, which is where remediation effort should
concentrate.

---

## Part 1 — Code Review

### 1.1 Critical (4) — all source-verified

| # | Where | Bug |
|---|---|---|
| C1 | `jit/src/aarch64_backend.rs:3596-3609` | `Ldr`/`Str` lowering uses the **pre-index writeback** form (`ldr_pre`/`str_pre`) for any negative or unaligned offset. Every frame slot is at a *negative* offset from FP, so each spill/reload mutates FP/SP → frame corruption. |
| C2 | `jit/src/aarch64_backend.rs:2543-2552` | `invokestatic` emits `BL` to an **unbound label**; the patch loop leaves it as "branch to self" (`BL .`) → infinite self-call / stack overflow at runtime. Should bail to interpreter until real call resolution exists. |
| C3 | `native-builtins/src/serialization.rs:1571` | **JEP-290 deserialization filter bypass.** The synthetic `readObject` path (`ois_read_object` → `ensure_class_initialized`) never calls `evaluate_serial_filters`; the filter is enforced *only* from the JDK `resolveClass` natives, which this path does not use. Gadget classes are not blocked. |
| C4 | `classloading/src/jar_signer.rs:2417` | **Cert-chain signature bypass.** `link_signature_ok` short-circuits on `sig_alg_oid == OID_STUB_SIG ("1.3.6.1.4.1.0.55")`, accepting `signature == SHA-256(tbs_der ‖ parent.subject_dn)` *before* any real crypto and *before* the trust store. The branch keys on an attacker-controllable cert field and is **not** `#[cfg(test)]`-gated — only a doc comment says "test fixtures only". |

> Note on C1/C2: aarch64 is the **secondary** backend (x86-64 is primary), so
> real-world blast radius is limited today — but both are hard miscompiles
> that make the ARM backend unsafe to enable. C3/C4 are security-critical and
> arch-independent.

### 1.2 Dominant recurring patterns (most of the High findings)

**Pattern A — cross-call state keyed by a raw object pointer, invisible to the
moving young-gen GC.** This is the single most common serious bug class. The
side-table survives a young GC move and then reads/writes a stale address
(silent data loss, UAF, lock corruption):

- `native-io/src/lib.rs:2157` — `BufferedReader` buffer table keyed by `this.as_ptr()` → stream data loss/corruption under read loops.
- `native-builtins/src/concurrent_extras.rs:362` — `SynchronousQueue` stores raw `ObjectRef` bits in a non-rooted global map → UAF under moving GC.
- `native-builtins/src/stamped_lock.rs:118` — `StampedLock` / `ReentrantReadWriteLock` tables keyed by raw heap address → unlock on the wrong/relocated object → deadlock/corruption.
- `native-builtins/src/lang_class.rs:6918` — `Proxy$Instance.getInterfaces()` returns a GC-stale raw pointer (`PROXY_LAST_INTERFACES_BITS`).
- `vm/src/native/jni.rs:613` — **JNI local references are not GC roots** and not remapped after a move (only globals are). Native-held locals can be collected or decay to stale pointers.
- `vm/src/vm/vm_exec.rs:153` — `coerce_value_for_return` reinterprets a `Value::Long` as `ObjectRef` with only an alignment check, no heap-membership test → GC marks a fabricated pointer → SIGSEGV (the validated sibling exists; this site doesn't use it).

**Fix direction:** key all cross-call native state on a GC-stable identity
(`identity_hash_code` + generation disambiguation, as `properties_sidetable`
/ `widened_obj_key` already do) **or** register the side-table as a scanned +
remapped GC root. This is worth a single shared helper + an audit sweep.

**Pattern B — attacker/caller-controlled length fed straight into an
allocation or loop (DoS / abort).** Negative `int` sign-extends through
`as usize` to ~1.8e19; huge `long`/`u64` requests exabytes:

- `native-builtins/src/serialization.rs:1529` — `TC_LONGSTRING` u64 length → multi-GB/EB `vec![0u8; len]` (no `filter_check_array`, unlike `ois_read_array`).
- `native-builtins/src/phases_late.rs:8129` — `RandomAccessFile.read([BII)` negative `len` → `vec![0u8; len]` abort (also `readFully`, `write`).
- `native-builtins/src/phases_late.rs:9825` — `FileChannel.transferTo/From` unbounded `count` → exabyte alloc.
- `native-builtins/src/servlet.rs:2277` — `ByteBuffer.get/put([BII)` negative len → ~1.8e19-iteration loop (hang).
- `native-builtins/src/net_phase_e.rs:5479` — `HttpServer` reads body to attacker `Content-Length` with **no cap** → remote memory-exhaustion DoS.
- `vm/src/debug/protocol.rs:91` — JDWP `read_packet` allocates `vec![0u8; data_len]` from a wire u32 before reading (≤4GB up-front alloc).

**Fix direction:** validate JDK preconditions (`off<0 || len<0 || off+len>arr.len`) with widened arithmetic and a hard size cap *before* allocating; clamp wire lengths to bytes-remaining.

**Pattern C — crypto that fakes / weakens guarantees** (see also C3, C4):

- `native-builtins/src/tls.rs:1495-1531` — default-build `X509TrustManager.checkServerTrusted/checkClientTrusted` returns `Ok(None)` (no date/signature/chain checks). *Nuance:* real connections go through `native-tls` which does PKIX validation; the gap is app-level custom `TrustManager`/`SSLContext` that expects the VM to validate.
- `native-builtins/src/crypto_impl.rs:1969` — RSA-OAEP / PKCS#1 unpad are **decryption oracles** (distinct error strings, non-constant-time).
- `native-builtins/src/crypto_impl.rs:1503` — `modpow`/EC scalar-mul are **labeled "constant-time" but are branchy/variable-time** (RSA private-exponent + ECDSA nonce leak).
- `native-builtins/src/phases_early.rs:9112` — PKCS7 unpad is non-constant-time (CBC padding oracle).
- `native-builtins/src/crypto_impl.rs:1013` — latent `pub` AES-ECB-only, zero-key-fallback `Cipher.doFinal` (dead but exported — delete or `#[cfg]`-gate).
- `native-builtins/src/phases_late.rs:27808` — `Mac` key material retained in an unbounded process-global table for VM lifetime.

**Pattern D — synthetic single-feature objects keyed on slot 0 / type-loose
equality** (the documented "synthetic collection landmine"):

- `native-collections/src/lib.rs:12121` — `natural_compare` orders custom `Comparable` by raw slot-0 field without the `is_wrapper` gate → wrong `TreeMap`/`TreeSet` ordering.
- `native-collections/src/lib.rs:2361` — wrapper-key equality not type-strict: `Integer(1)` aliases `Long(1)`/`Short(1)`/`Character(1)` as the same `HashMap` key (Java says these are unequal).

**Pattern E — stubs that silently return wrong results** (top of 47-item list):

- `native-builtins/src/jdk25_concurrency.rs:504` — `StructuredTaskScope.fork()` runs Callables **synchronously**; `join()` is a no-op → deadlocks dependent subtasks, zero parallelism.
- `native-builtins/src/phases_late.rs:12473` — `SynchronousQueue` is a non-blocking, non-thread-safe single slot; `size/isEmpty/peek` hardcoded.
- `native-builtins/src/http2.rs:503` — `HttpClient.send/sendAsync` **silently drop all request headers and the body** (POST sends nothing); response headers fabricated empty.
- `native-awt/src/natives.rs:2054` — `FontMetrics` are flat `size*0.55` heuristics decoupled from the real glyph renderer → all Swing text mismeasured.

### 1.3 Other notable High findings

- `native-builtins/src/lang_string.rs:4148` — `String.indexOf/lastIndexOf/getChars/regionMatches` operate on Unicode **code points, not UTF-16 code units** → every index off by # of preceding supplementary chars; `getChars` can corrupt the dest array.
- `native-builtins/src/phases_early.rs:1864` — `Properties.setProperty` silently drops the **17th** distinct key (fixed 32-elem array, no growth; the OOB store error is discarded by `vm_exec.rs:2089`).
- `native-io/src/lib.rs:6097` — `FileChannel.write/read` **swallow I/O errors**, report success, advance position → silent data loss.
- `native-io/src/nio_selector.rs:1309` — `Selector.select(0)` busy-spins (polls) instead of blocking indefinitely (JDK semantics violation; idiomatic event loops spin).
- `classloading/src/bytecode_verifier.rs:271` — verifier never merges the **branch-edge** type-state into the target frame (only fall-through) → type-confusion hole for branch-only targets.
- `classloading/src/class_path.rs:1797` — unsigned/undeclared JAR entries **inherit the signer's certs** (JAR-spec unsigned-entry attack).
- `gc/src/gen_heap.rs:5472` — `walk_objects` young-gen branch isn't free-list / `GAP_FILLER` aware → linear-walk desync on a post-sweep young space.
- `gc/src/class_unloading.rs:387` — `is_ancestor` can infinite-loop on a cyclic parent chain (GC-thread hang).
- `gc/src/concurrent_mark.rs:529` — concurrent sweep can free objects the SATB barrier left unmarked after the gate flips `INACTIVE` too early (floating-garbage free → UAF).
- `vm/src/native/jni.rs:2228` — `Get/Release<Type>ArrayElements` re-derives the buffer length at Release time → `Vec::from_raw_parts` length/capacity mismatch → heap corruption.
- `jit/src/x64.rs:2730` — `array_receiver_local` mis-decodes multi-byte index pushes when scanning backward → wrongly elided null check → SIGSEGV on null array.
- `jit/src/ir_lower.rs:404` — `Op::Cmp` emits `cc - 0x10` (MMX `PCMPEQB`) instead of `cc + 0x10` (`SETcc`); `Phi` slots (`:423`) are allocated but never written by predecessors. Both behind the default-off reassoc/IR gate, but live bugs in that path.
- `jfr/src/repository.rs:857` — per-thread JFR ring shards are never unregistered → unbounded leak + O(dead_threads) drain.
- `cuda-bridge/src/backend_cuda.rs:391` — default-stream pipeline races on a context-wide singleton `e_h2d` event under concurrent use (input-side analogue of the already-fixed H10b output-side bug).
- `native-awt/src/image.rs:305` — every `BufferedImage` leaks its full raster for the VM lifetime (no runtime reclamation path).

### 1.4 Performance (54 items — representative)

- `reader/src/signature.rs:485` — LRU `order` deque never deduplicates → premature eviction of hot keys + unbounded growth.
- `reader/src/signature.rs:164`, `field_type.rs:79` — fresh `String` alloc per identifier/class-name even though CP holds interned `Arc<str>` (bootstrap alloc churn).
- The JIT is effectively **single-tier** (`interpreter.rs:13942` consults the tiered manager but compiles one tier at a fixed threshold) — see Features.
- Numerous "decode to `Vec<char>` then index" String intrinsics (also a correctness bug, §1.3) allocate per call.

---

## Part 2 — Tests Review

**Aggregate:** strong where it counts (parser, JIT, GC, classloading, jfr,
native-io, native-awt) and weak in the mega-crates. Several suites are
**currently RED** — fix those first; a red suite erodes trust in the rest.

| Crate | Est. coverage | Adequacy | Notes |
|---|---:|---|---|
| reader | ~72% | good | 297 tests, no `unsafe`; lib test target doesn't compile (missing `HelloWorld` fixture). |
| types | ~92% | excellent | |
| native-api | ~62% | good | bulk-array default impls, `native_ring`, socket-option surface untested. |
| native-collections | **~29%** | **poor** | measured with llvm-cov; 3 RED tests; streams/collectors (3.4K LOC) have zero behavioral tests. |
| native-io | ~72% | good | 287 tests, 1 timing flake. |
| native-builtins | ~52% | fair | 3067 `#[test]`s but registry-presence-only in the 3 dispatch megafiles; xml/JCA gaps. |
| native-awt | ~72% | good | platform FFI (66 `unsafe`) untested. |
| jit-api | ~72% | fair | **3 RED tests** (stale field counts: `NUM_FIELDS` is 41, tests say 40/33). |
| jit | ~82% | good | excellent; no aarch64 *execution* tests (would have caught C1/C2), no codegen fuzz. |
| jit-cuda | ~45% | fair | **31/42 tests FAIL** — fixtures (`test_classes/gpu/*.java`) not git-tracked. |
| cuda-bridge | ~40% | fair | all tests run against the *stub* backend only. |
| classloading | ~84% | good | best-tested; verifier/JAR parsers lack fuzzing. |
| craton-gpu | **0%** | poor | build-only crate, zero tests. |
| gc | ~72% | good | **4 RED tests** (finalization-queue + order-fragile); 533 `unsafe` w/o invariant tests. |
| vm | ~58% | fair | vm.rs's 1,520 tests gated behind non-default `synthetic-jdk`; `jit/helpers.rs` 124 `unsafe`/29 tests. |
| vm-cli | ~42% | good | `run()` orchestration + `--jar` path uncovered. |
| jfr | ~82% | good | no Miri/loom on the unsafe SPSC ring. |
| fuzz | ~70% | good | no CI runs it; no seed corpus; workspace-membership contradiction. |

**Is coverage ≥85%?** No — only `types` (92%) and `classloading` (84%, ~at
target) approach it. The weighted whole-project figure is well under 85%,
dragged down by `native-collections` (29%), `vm` (58%), `native-builtins`
(52%), and the GPU crates.

**Top test recommendations**
1. **Fix the RED suites now:** `jit-api` (3), `native-collections` (3), `gc` (4), `reader` lib-target fixture, `jit-cuda` fixtures (git-track `test_classes/gpu/*.java`).
2. **Wire fuzzing into CI** — the `fuzz` crate exists but nothing runs it. Add no-panic targets for the bytecode verifier, JAR/ZIP/manifest parsers, PEM/DER, classfile reader, JDWP/serialization wire readers, JIT `is_jit_compatible`+`compile`. This directly attacks Pattern B.
3. **Behavioralize registration-only tests** in the dispatch megafiles using the existing `MockNativeContext`.
4. **Add a moving-GC differential harness** for every raw-pointer side-table (Pattern A): allocate → force young GC → assert the native state survives.
5. **De-gate `vm.rs` object-model tests** so they run in the default build; add a `proptest` for `memory/gc.rs::update_all_roots`.
6. **Miri/loom** for the jfr SPSC ring, ConcurrentHashMap stripe locks, monitor inflation, class-init state machine.

---

## Part 3 — Docs & Scripts

### 3.1 Documentation consistency
- **G1/ZGC contradiction (most serious):** README + CONTRIBUTING say "experimental zgc-gated stub, NO G1", but `gc/src/g1.rs` is a real `GcAlgorithm` variant with a full implementation, and ZGC is the feature-gated stub. ARCHITECTURE.md/CHANGELOG/code disagree with README/CONTRIBUTING. Fix the README/CONTRIBUTING blurb.
- `PLATFORMS.md` ARM claim vs the (broken) aarch64 backend — temper claims.
- `docs/README.md` and `../fixed-suite-bugs/README.md` link to `../ARCHITECTURE.md` etc.; no `ARCHITECTURE.md` exists *under docs/* (it's at repo root) — links resolve, but there's no public/internal index split.

### 3.2 Job docs to relocate into `docs/internal/`
These are per-app/per-suite job-tracking artifacts (same class as what's
already under `docs/internal/`), not public docs:

| Move → `docs/internal/` | What it is |
|---|---|
| `docs/h2-suite-bugs/` | H2 suite bug/triage reports |
| `docs/kafka-suite-bugs/` | Kafka bug-01..27 + sweep summaries + `repro19/` |
| `docs/keycloak-crash-reports/` | numbered Keycloak crash triage |
| `docs/known-issues/` | CratonVM-only defect map (SB-CRASH-04, fork6, …) |
| `docs/tomcat-suite-bugs/` | Tomcat per-bug reports |
| `docs/wildfly-suite-bugs/` | WildFly bug-01..07 + embedded repro probes |
| `docs/gaps/` | open-gap index from cross-VM comparison runs |
| `docs/reviews/` | round-N / dated review notes |
| `docs/bc-jit-ban-investigation.md` | point-in-time BC JIT-ban investigation |
| `docs/synthetic_methods.md` | borderline — internal native/stub-tier census |

**Decision needed:** `docs/internal/` is itself **currently git-tracked (199
files)**. If the intent is "out of the public repo," `docs/internal/` should
be `.gitignore`d (and `git rm --cached`). Otherwise the move just reorganizes
within the tracked tree. (See cleanup question at end.)

### 3.3 Cruft to remove from git (tracked, shouldn't be)
- `build-cpu-log.txt`, `build-gpu-log.txt` — UTF-16 build-console dumps full of `C:\craton\…` paths; **not** matched by current `.gitignore` (`build-*.log/.out` miss `.txt`).
- `build-cpu-bj.bat`, `check-bj.bat` — hardcode `CARGO_TARGET_DIR=C:\craton\cratonvm-bjtarget` (author-only).
- `build-cpu-evade.bat`, `check-cv20.bat` — invoke **renamed toolchain binaries** under `C:\Users\Victor\.rustup\…` (leaks dev home; "taskkill-evasion" hack).
- `h2sweep/` — ad-hoc triage scratch (hardcodes `C:\craton\…`, `jdk-25`).
- **Keep:** `build-cpu.bat`, `build-cpu-rwd.bat` (canonical, referenced), `spring-suite/`, `wildfly-suite/` (doc-referenced fixtures).
- Already-ignored & untracked (no action): `applogs/`, `output/`, `scratch/`, `target/`, `target-gpu/`, `hs_err_pid*.log`.
- Close `.gitignore` gaps so the above can't return (`build-*-log.txt`, the variant `build-cpu-*.bat`/`check-*.bat`).

### 3.4 Scripts
The curated tier (`scripts/`, `ci/`, `.github/workflows/`, most
`test-infra/*.sh`) is good: relative paths, env-var overrides, parallel
bash/PowerShell ports kept in sync. The 10 root-level `*.bat`/`*.sh` are the
throwaway tier (above). Minor: a few `bench/**/stage-*.ps1` and one
`native-io` test hardcode `C:\craton` layout paths — parameterize.

---

## Part 4 — OSS / crates.io readiness (Apache-2.0, Craton Software Company / craton.com.ar)

| Area | Verdict | Detail |
|---|---|---|
| **License hygiene** | **ready** | LICENSE = full Apache-2.0 with correct "Copyright 2024-2026 Craton Software Company"; NOTICE/AUTHORS/THIRD-PARTY-NOTICES/TRADEMARKS all present & consistent. |
| **crates.io publish** | **close** | All 18 `cratonvm-*` names are AVAILABLE (404 on crates.io) — name-squatting is a **non-issue** thanks to the prefix. Metadata complete; path deps carry `version=0.3.0`; `cargo publish -p cratonvm-types --dry-run` succeeds. Remaining work: publish in dependency order; CI publish-order gate. |
| **Secrets / privacy** | **needs-work** | Working tree is clean of live credentials. Blockers below. |
| **Repo hygiene** | **close** | All community files present and high-quality (CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, GOVERNANCE, MAINTAINERS, SUPPORT, RELEASING, CHANGELOG, ROADMAP, .github/). |

**Secrets/privacy blockers (history rewrite required before going public):**
1. **HIGH — 595 MB `crash.dmp` in git history** (added `0e8a2e3c`, removed `c3014f2c`). Not in tree but recoverable from any clone; crash dumps contain raw process memory (env vars, keys). Purge with `git filter-repo`/BFG.
2. **MEDIUM — personal dev scripts** `build-cpu-evade.bat`, `check-cv20.bat` leak `C:\Users\Victor\…` toolchain paths (also §3.3).
3. **MEDIUM — committed TLS private keys** in `native-builtins/src/t27_certs/` (4 RSA-2048 `.key` + 5 self-signed certs). Verified throwaway test fixtures, but decide policy: keep + allowlist in secret-scanning, or regenerate at test runtime.
4. **LOW** — `C:/Users/Victor/.m2` in two `docs/gaps/*.md` run-commands; `.pdb`/`.sym`/trace-log blobs in history (clone bloat); 576/1806 commits use personal emails (`@ois.gold`, `@yandex.ru`) instead of `@craton.com.ar`.
5. **LOW** — LICENSE omits the canonical "APPENDIX: How to apply…" boilerplate (replaced by the real Craton stanza — operative terms are intact, cosmetic).
6. **LOW** — repo-name casing mismatch (`cratonvm` vs `CratonVM`); CI guards are advisory, not blocking.

**Recommended before first public push:**
- One `git filter-repo` pass to purge `crash.dmp`, `.pdb`/`.sym`, and large trace logs from **all** history (irreversible — do it once).
- Enable GitHub push-protection / gitleaks pre-push hook.
- Add `cargo-deny` (`deny.toml`) + `cargo-about` license gate to CI (currently a manual audit; 355 deps clean today).
- Decide the `t27_certs/` policy and genericize the `.m2` doc paths.

---

## Part 5 — Feature directions

**Runtime / performance (the highest-leverage structural work):**
- **Real frame-state deopt** (materialize the interpreter frame at the trapping bci) — today deopt is a sentinel that re-runs the whole method. This is the *keystone* that unlocks everything below. (XL/high)
- **Speculative type/null guards backed by deopt** — the tiered manager already collects `Receiver/Type/Null` profiles but the live JIT ignores them. (L/high)
- **Wire the dormant tiered manager** (`jit/src/tiered.rs` has a full 5-level policy + priority queue that `interpreter.rs` throws away) → real C1 + profile-driven C2. (L/high)
- **Default moving/compacting young gen** to close the Binary-Trees-18 ≈23× gap (selective-promotion work already exists). (XL/high)
- **Background compilation thread**; **aggressive inlining once deopt is safe**; **finish escape-analysis → scalar replacement**; **activate the IR optimizer** (GVN/LICM/DSE are mostly dormant). (M–L)
- Lower-pause concurrent marking as default old-gen path (note: mark worklist *panics* at 1M entries — a DoS on `Object[1.5M]`).

**Java compatibility (the "no synthetic stubs" goal):**
- **Stub-ratchet CI gate** on the existing `NativeKind` census (`--dump-native-registry`) — fail PRs that *add* `SyntheticStub`s. Cheap, high-leverage. (S/high)
- **Synthetic-object eliminator** — convert `alloc_concurrent_synthetic` sites to real classfile-backed objects (kills the recurring slot-0/comparator divergence class). (L/high)
- **Real-bytecode CDI/bean container** to retire the framework-shim cluster (quarkus_arc, spring_startup_bootstrap, wildfly_core, jboss_msc, …). (XL/high)
- **Fix the non-TTY `System.console()` SEGV/hang** gating Spring Boot / Felix / kc26 daemons. (M/high)
- **`Proxy.newProxyInstance` that synthesizes a real class file** (not name-lookup dispatch). (L/high)
- **PKCS12/JKS KeyStore + finish RSA/ECDSA/ML-DSA/ML-KEM real-provider coverage** (`java_security` is the lowest JCK row at 68%). (L/high)
- JEP-358 helpful NPE messages; real `StringConcatFactory`; JVMTI `ClassFileLoadHook`+retransform for ByteBuddy/JaCoCo-class agents.

**DevEx / ecosystem:** productize the already-deep internals (JFR, JDWP,
JVMTI, serviceability, GPU offload) into externally-credible, verified
tooling — an embedding API (`libcratonvm`), a continuous differential-vs-HotSpot
harness wired into CI (the `divergence-log.md` has only 3 entries), and a
published benchmark/conformance dashboard.

---

## Appendix — suggested remediation order

1. **Security first (public-repo blockers):** C3, C4, the TLS TrustManager gap, the serialization/JDWP/JAR DoS+filter items; purge `crash.dmp` from history; decide `t27_certs/` policy.
2. **Pattern A sweep:** one GC-stable-identity helper + audit every raw-pointer side-table (native-io, concurrent_extras, stamped_lock, lang_class, JNI locals, `coerce_value_for_return`).
3. **Pattern B sweep:** length-validation + size caps at every wire/arg length read.
4. **Fix all RED test suites; wire fuzz + a moving-GC side-table differential into CI.**
5. **Docs/cruft cleanup** (§3.2/§3.3) + the OSS hardening (cargo-deny, push-protection).
6. **Runtime roadmap:** real deopt → tiering → moving young gen.
