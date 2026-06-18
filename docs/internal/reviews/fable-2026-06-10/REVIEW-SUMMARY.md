# CratonVM — Consolidated Review Summary (Fable, 2026-06-10)

Multi-agent per-module review of the whole workspace. One agent per module;
full per-module reports live beside this file (`<module>.md`). This document
synthesizes them against the five requested axes. **Static review only — no
builds/tests were run.**

Coverage of the review itself: all 20 code modules + docs + scripts +
oss-readiness reviewed. The three cross-cutting reports (docs, scripts,
oss-readiness) were completed inline after their agents hit the session limit.

---

## 1. Code review — bugs, vulnerabilities, stubs, performance

### Severity note
**No finding rose to CRITICAL.** The top tier is HIGH. The codebase is, across
the board, **defensively engineered and heavily pre-audited** (reader, gc,
classloading, vm-core, vm-runtime all show checked-arithmetic discipline on
untrusted input and "never panic, bail to interpreter" contracts). The HIGH
findings are concentrated, specific, and fixable.

### Top HIGH findings (memory-safety / soundness first)

| # | Module | Finding | Why it matters |
|---|--------|---------|----------------|
| 1 | **jit** | BCE off-by-one for **inclusive** loop comparators (`for i=0;i<=n;i++) a[i]=…`): `analyze_loop_bound` accepts `if_icmpgt`/`if_icmple` but records only the bound, so the header guard proves only `length>=bound` → the per-element check is elided on a one-past-the-end index → **OOB heap read/write**. `jit/src/x64.rs:4044,11026,12182`. Reachable from **ordinary javac output**, worse-than-panic in release. | Memory safety, benign trigger |
| 2 | **native-collections** | Fast-mode TreeMap values are **not GC roots and never remapped** → use-after-free / stale pointers after a young GC (B1/V1). | Memory safety |
| 3 | **native-collections** | `natural_compare` returns 0 for any `Comparable` → TreeMap/TreeSet ordering silently collapses (B2); `Collections.sort(List)` only sorts by *string* key (B3). | Silent wrong results (wrong-intrinsic stubs) |
| 4 | **nb-security** | RSA PKCS#1 v1.5 encode **underflows on small keys → panic/OOM**, reachable from `checkServerTrusted` (B1); DER length-field **integer overflow → slice panic** on a malicious certificate (B2). | Remote DoS from hostile cert |
| 5 | **nb-security** | HTTP/1 chunked decode + HTTP/2: chunk-size overflow and **pre-cap unbounded allocation** → DoS from a malicious server (B3). | Remote DoS |
| 6 | **classloading** | Signed-JAR verification **never checks per-entry manifest digests** (signature trust incomplete, V1); zip-bomb defense caps pre-alloc but not the streaming read → **decompression-bomb DoS** (V2). | Trust/Dos completeness |
| 7 | **vm-runtime** | JIT array load/store helpers **silently swallow OOB** instead of throwing `AIOOBE` (B1, HIGH-as-latent). | Soundness asymmetry |
| 8 | **nb-rest** | `KeyStore` alias lookups always fail — `read_string_arg` is a **no-op placeholder** (B1). | Broken feature masquerading as impl |
| 9 | **jit** | `find_modified_locals` unclamped `1u64<<local` for `max_locals>64` → debug panic / release wrong-mask that **re-opens finding #1** (B2, MEDIUM). | Contract + soundness |
| 10 | **vm-cli** | `-Xms <size>` separate-token form mishandled → corrupts the argv pipeline (B1). | CLI correctness |

Latent (not currently wired, flag so it can't be adopted): **nb-security V1** —
`crypto_impl.rs::verify_cert_chain` trusts intermediates/anchors by **CN string
equality** (test-only path; would be a full bypass if ever wired to TLS).

### Stubs / unimplemented — incl. *forbidden* synthetic app-faking
The project policy ("no synthetic stubs that fake app behavior") is **violated in
several places that are ON by default or unconditionally registered**:

- **vm-core** — `<clinit>` failure is **swallowed** (class marked Initialized) for
  a broad framework allowlist, then `post_clinit_fixup` **fabricates static state**:
  `LogManager.manager`, a **pass-through (no-op) Unicode normalizer**, VarHandle
  `FORM`, Quarkus `DELAYED_HANDLER`, JBoss module-loader, Spring `ApplicationStartup`,
  WildFly `ElytronMessages`, JBoss `ServiceLogger`. Diverges from JVMS §5.5; **default-on**
  (only `CRATONVM_STRICT_SWALLOWS=1` disables). `vm/src/vm/vm_util.rs:754-2289`.
- **nb-rest S1** — `log4j_extras.rs` is a **silent no-op logging pipeline**,
  unconditionally registered, **not even tagged** `SyntheticStub`.
- **nb-rest S2** — `apps_h2.rs` **reimplements `org.h2.table.TableFilter.prepare()`
  in Rust** to mask a VM optimizer bug (app-specific shim).
- **nb-rest S3** — `classfile_api.rs` **fabricates the entire JEP 484 Class-File API**.
- **nb-security** — default JCA **PQC (ML-KEM/ML-DSA) keygen returns synthetic/empty
  key material**; HPACK **Huffman decode is unimplemented** (correctness stub).
- **native-collections B2/B3**, **nb-rest B1**, **native-api** (charset "best-effort"
  natives returning plausible-but-wrong output) — wrong-constant / placeholder natives.
- **gpu** — a large pile of intentional, *documented* stub/no-op surfaces (stub CUDA
  backend, unfinished PTX lowering shapes) — acceptable but should be inventoried;
  plus a dead hardcoded NEON detector in the non-production aarch64 backend.

### Performance opportunities (highest-leverage)
- **native-collections P1** — `LinkedBlockingQueue` poll is **O(n) per element →
  O(n²)** FIFO (shifts the whole array each dequeue); use a circular head/tail.
- **nb-rest P1** — blocking `socket.accept()` holds a **process-wide read lock for
  its entire duration** (serializes the VM under any accepting server).
- **vm-core** — `read_java_string` decodes LATIN1/UTF16 byte arrays **element-by-element**
  (one of the hottest VM functions); add a bulk read like the char path already has.
- **jit** — per-loop-header `filter().cloned().collect()` over BCE guards; `dup2`
  linear metadata scans; per-call `env::var_os` on the BCE path (cache in `OnceLock`).
- **vm-core** — `safe_native_call` pays `catch_unwind` setup + per-arg pin on every
  native dispatch; a proven-safe fast path would help the central choke point.

### Reassuring (low risk)
- **gc** — *no* critical/high correctness or soundness bug; collectors and the
  free-list/mark-bit ordering invariant are correct and well-commented.
- **reader** — well-hardened against untrusted classfiles (checked_add, recursion
  caps, count validation); findings only low/medium.
- **types** — clean; also the only module with strong tests.

---

## 2. Tests — do they reach 85%?

**No. Only `types` (~92%) clears the 85% bar.** Every other module is below it,
many far below. Best-estimate per-module line coverage (from the reviewers'
reading, not a coverage tool):

| Module | Est. | | Module | Est. |
|---|---|---|---|---|
| types | **92%** ✅ | | native-io | 62% |
| jfr | 80% | | vm-threading | 62% |
| reader | 78% | | vm-cli | 62% |
| jit | 70% | | nb-security | 55% |
| gpu | 70% | | vm-core | 55% |
| native-api | 70% | | vm-runtime | 55–60% |
| nb-lang | 70% | | nb-rest | 52% |
| classloading | 70% | | nb-appserver | 45% |
| gc | 62% | | native-collections | **35%** |
| native-awt | 62% | | nb-core | **22%** |

**Workspace risk-weighted average ≈ 60%.** The worst gaps are also the
highest-risk surfaces: `native-collections` (35% — *zero* behavioral tests for
streams, Collectors, Optional, Comparator, sort, the concurrency collections, and
the GC-overlay soundness functions) and `nb-core` (22% — most of the registration
surface untested). The two memory-safety HIGH findings (jit BCE inclusive-loop;
native-collections TreeMap GC roots) are **both in untested paths**.

**Most important tests to add** (would also pin the HIGH bugs):
1. jit: inclusive-comparison BCE at `array.length == bound` (boundary deopt/result);
   `find_modified_locals` with `local >= 64`.
2. native-collections: TreeMap/TreeSet ordering + GC-overlay scan/remap end-to-end;
   stream/Collectors/Optional behavioral suite.
3. nb-security: malformed-cert fuzz for `der_read_*`/`parse_der`/`read_chunked`/H2
   frame loop/HPACK; assert `checkServerTrusted` rejects small-RSA chains without
   panicking; JCA fail-closed on unimplemented algorithms.
4. classloading: per-entry digest mismatch rejection; real high-ratio deflate bomb.
5. vm-cli: split `run()` into a parsed-config struct and table-test argv (incl. B1).

---

## 3. Documentation & scripts

See `docs-review.md` and `scripts-review.md`. Headlines:

- **Maintained user-facing docs are strong and honest** (README "Known
  Limitations" is candid). But **9 of 12 spot-checked doc links are broken**
  (`BUILD_GUIDE.md`, `TRADEMARKS.md` don't exist; many `docs/X.md` links actually
  live under `docs/internal/`).
- **RELEASING.md contradicts `Cargo.toml`** on the publish gate (says
  `publish=false` is set workspace-wide; it is not — every crate is publishable).
- **CI is described as active but is dormant** — workflows are parked under
  `.github/.wf/` (Actions only reads `.github/workflows/`), so the README badge is
  broken and "CI enforces X" in CONTRIBUTING/RELEASING is false. *Single
  highest-leverage infra fix: move `ci.yml` into `.github/workflows/`.*
- **Internal vs external:** the `docs/` external set + root governance files are
  shippable; `docs/internal/**` (~150 files of round logs, blocker maps, and 18
  `continue_prompt_*` session handoffs) is honestly marked non-normative but
  should be triaged/scrubbed (machine paths) or extracted before release.
- **Scripts** are an internal dev toolbox (hardcoded `C:/craton/...`, a personal
  `C:\Users\Victor\...hmrustc.exe` toolchain in `build-devverify.bat`, three
  `taskkill //F //IM cratonvm.exe` that kill unrelated processes). **No secrets in
  scripts** (clean). `fuzz/` appears to have no committed fuzz targets — confirm or
  drop.

---

## 4. Open-source / crates.io readiness (Apache-2.0, Craton Software Company)

See `oss-readiness.md` (full detail). The Cargo metadata is genuinely
well-prepared (every crate has description/license/repository/readme/keywords/
categories; all path-deps carry `version = "0.3.0"`; `.class` fixtures excluded
from publish). **Release blockers:**

- **Committed TLS private keys** in the tree (security-scanner + published-crate
  content). Must be removed/rotated and history-scrubbed.
- **Vendored LGPL Hibernate file at repo root** — license-incompatible with a
  clean Apache-2.0 release; remove or isolate.
- **Verbatim BouncyCastle-derived ports** (e.g. NewHope precomp tables, ChaCha
  cores) are **mislabeled as sole Craton copyright with no attribution** — add
  `THIRD-PARTY-NOTICES.md` + NOTICE entries (BouncyCastle MIT-style license).
- **Publish gate wide open** — no crate sets `publish=false` except `fuzz`; immature
  crates (cuda-bridge, jit-cuda, craton-gpu, native-awt) would publish. Decide the
  per-crate policy; reconcile RELEASING.md.
- **CI dormant + broken README badge** (as above).
- **docs.rs risk:** `craton-gpu` and `jit-cuda` build scripts invoke `javac`; their
  docs.rs builds will fail unless guarded.
- **Personal machine paths** (`C:/Users/Victor`) committed in a script + two
  `docs/gaps/*`.
- **Domain inconsistency** — `craton.co` vs `craton.com.ar` across
  SECURITY.md/MAINTAINERS/homepage; owner brief says `craton.com.ar`. Pick the
  canonical domain; point `homepage` at the company site.
- Minor: missing `TRADEMARKS.md`; `craton-gpu` lacks `[lints] workspace = true`;
  add an SPDX header to `gc/src/shadow_stack.rs`; add `deny.toml` + a `cargo-deny`
  CI job; verify the 17 `cratonvm-*` names are free on crates.io and reserve them;
  run a leaf-first `cargo publish --dry-run` across the workspace first.

---

## 5. Feature / direction suggestions (cross-cutting)

1. **Close the soundness gaps before anything else** — the jit inclusive-loop BCE
   and the native-collections TreeMap GC-root bugs are real memory-safety holes
   reachable from ordinary code; they undercut the "written entirely in Rust /
   memory-safe" value proposition.
2. **A coverage gate + `cargo-llvm-cov` in CI**, with a floor that ratchets up.
   Today only `types` would pass an 85% bar; make coverage visible.
3. **Finish or formally fence the synthetic-stub surface** — tag every remaining
   stub with the `NativeKind`/`SyntheticStub` marker (nb-rest S1 isn't), and make
   the `--dump-native-registry` census fail CI on any unfaked-but-untagged native.
   Turn the vm-core `<clinit>`-swallow **off by default** (invert
   `CRATONVM_STRICT_SWALLOWS`).
4. **Hostile-input fuzzing as a product feature** — wire the `fuzz/` crate with
   real targets for the classfile reader, DER/cert parser, and HTTP/zip decoders
   (the DoS findings cluster there); run under OSS-Fuzz.
5. **JAR-main-class auto-detection** and **basic `java.sql`/JDBC** are the two
   gaps most visible to new users (README lists both as limitations).
6. **Productionize one collector story** — docs claim semi-space/G1/ZGC; reality is
   a generational collector + a ZGC stub. Either ship the stub honestly as
   experimental or invest in a concurrent collector (already on the roadmap).

---

### Index of per-module reports
`reader` `types` `native-api` `native-collections` `native-io` `native-awt`
`nb-core` `nb-lang` `nb-security` `nb-appserver` `nb-rest` `jit` `gpu`
`classloading` `gc` `vm-core` `vm-runtime` `vm-threading` `vm-cli` `jfr`
`docs-review` `scripts-review` `oss-readiness`
