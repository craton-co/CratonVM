# native-builtins reachability map — 2026-07-26

Scope: `native-builtins/src/**`. Base: `arch/wave1-integration-20260726` merged into
this worktree at `18b2f49d5e1b90857acb3aa0fb5c905918f962d3`.

Purpose: answer, file by file, **what of this crate is live on the default
(real-JDK) build**, so the `native-essentials` split proposed in
[`jdk-mode-determinism.md`](jdk-mode-determinism.md) §7.4 step 2 can be costed
before anyone attempts it.

The short version: **the split as proposed does not shrink the default build.**
The real-JDK arm reaches 91 of 167 modules at *one hop* and 158 of 167
transitively. What is genuinely synthetic-only is 8 modules and ~14 kLoC.

---

## 0. Correcting the headline LoC figure first

`native-builtins` is quoted as **569,510 LoC / 43.5 % of the workspace**. That
number counts a vendored third-party crate:

| Path | LoC | What it is |
|---|---:|---|
| `native-builtins/src/**` | 521,377 | CratonVM code (167 `.rs` files) |
| `native-builtins/vendor/rustls-cbc/**` | 47,858 | **vendored third-party crate**, not CratonVM code |
| `native-builtins/{tests,examples}/**` | 275 | integration tests + one example |
| **total** | **569,510** | |

Everything below is about the 521,377 LoC in `src/`. Of that, **65,851 LoC
(12.6 %) is `#[cfg(test)]`** and never ships in any build.

---

## 1. How reachability is actually decided (it is not `cfg`)

The assumption that "~120 `#[cfg(feature = "synthetic-jdk")]` sites compile out"
is wrong by an order of magnitude. There are **41** synthetic-jdk `cfg` forms in
`native-builtins/src`, gating **3,940 LoC — 0.8 % of the crate**. Two of them
(`lib.rs:9140`, `lib.rs:9196`, the `BufferedInputStream` bridge) are *inside*
`register_essential_natives` as `if cfg!(...)` runtime-shaped gates.

Essentially the whole crate **compiles into the default binary regardless of the
feature**. The feature is a *registration-time* switch, not a compilation switch:

| Entry point | Location | Default (real-JDK) build |
|---|---|---|
| `register_essential_natives` | `lib.rs:6851–19477` (12,627 lines) | **called** — `vm_init.rs:1509` |
| `register_builtins` | `lib.rs:19950–22519` (2,570 lines) | `#[cfg(feature = "synthetic-jdk")]`; replaced by a no-op shim at `vm/src/native/builtins.rs:24` |
| `register_synthetic_overrides` | same | no-op shim, `vm/src/native/builtins.rs:29` |

So "is module X live?" reduces to "does the `register_essential_natives` /
`vm_init` real-JDK-arm call graph reach it?" — not to any `cfg`.

### What `register_essential_natives` actually is

| Measure | Value |
|---|---:|
| Lines | 12,627 |
| Direct `registry.register(` sites | **960** |
| Distinct `register_*` callees | **184** (183 in-crate + `register_io_natives` from `native-io`) |
| In-crate modules reached at **one hop** | **91** |
| LoC in those 91 modules | **357,075** |

The "~300 natives in real-JDK mode" figure repeated in older docs and in
`vm/Cargo.toml:31` ("the ~150 essential natives") is off by roughly an order of
magnitude even before the 183 sub-registrars are expanded.

---

## 2. LoC-per-bucket reachability table

Method: static call graph over every `fn` in `native-builtins/src`, rooted at
(a) `register_essential_natives`, (b) every `register_*` the non-`synthetic-jdk`
arm of `vm_init.rs` calls (53 symbols; the arm itself is `vm_init.rs:1493–2078`,
scanned through `:5973` to also pick up the unconditional registration code that
follows it and therefore runs in both modes), and (c) every
`cratonvm_native_builtins::*` path referenced from `vm`, `jit`, `gc`,
`classloading`, `native-io`, `native-collections` (GC root scanners, intrinsic
lookup, `classloader::*` hooks — 171 root symbols, 162 resolved in-crate).
Synthetic-only = reachable **only** from `register_builtins` /
`register_synthetic_overrides`, or defined under a `synthetic-jdk` `cfg`.

### By bucket (function-body LoC, so `#[cfg(test)]` is separated out)

| Bucket | LoC | Share |
|---|---:|---:|
| **Always-registered (real-JDK path)** | **321,172** | 67.9 % |
| **Synthetic-only** | 74,743 | 15.8 % |
| `#[cfg(test)]` | 65,851 | 13.9 % |
| Unreached from either root set | 10,999 | 2.3 % |
| *(total fn-body LoC)* | *472,765* | |

The "unreached" bucket is mostly an artefact of static analysis (helpers reached
through trait objects, macro expansion, or `fn` pointers stored in tables), not a
deletion list. A stricter edge rule (calls and `::path` references only, no bare
identifiers) moves the split to 41.8 % / 14.8 % / 29.5 % — the two runs bracket
the truth. **Both agree the synthetic-only share is ~15 %.**

### By cluster (whole-file LoC)

| Cluster | Files | File LoC | Real | Synth-only | Test | Unreached |
|---|---:|---:|---:|---:|---:|---:|
| JDK core surface | 66 | 174,698 | 94,429 | 25,446 | 26,618 | 3,093 |
| `lib.rs` + `phases_early` + `phases_late/**` | 16 | 118,150 | 84,380 | 41,287 | 3,651 | 1,503 |
| crypto / TLS / net | 32 | 91,573 | 54,899 | 4,150 | 15,230 | 2,225 |
| app shims: Spring / H2 / ANTLR / Lucene / JMX | 18 | 66,678 | 53,225 | 1,731 | 5,690 | 1,034 |
| app shims: JBoss / WildFly / XNIO | 25 | 48,223 | 27,688 | **5** | 8,140 | 1,199 |
| app shims: Quarkus / GraalVM / pools | 8 | 13,746 | 3,459 | 1,332 | 4,637 | 849 |
| test scaffolding | 2 | 8,309 | 3,092 | 792 | 1,885 | 1,096 |
| **total** | **167** | **521,377** | **321,172** | **74,743** | **65,851** | **10,999** |

Two things stand out.

1. **The synthetic-only mass is concentrated in exactly two places**:
   `phases_early.rs` (17,734 synth-only LoC against 11,223 real in the *same
   file*) and `phases_late/**` + `lib.rs`. It is not distributed across the app
   shims — the JBoss/WildFly/XNIO cluster is 48 kLoC of which **5 LoC** is
   synthetic-only.
2. **The app shims are all real-path live.** 128 kLoC of WildFly / XNIO /
   Undertow / Quarkus / Spring / H2 / ANTLR / Lucene / CGLIB / Hibernate glue is
   unconditionally registered on the default build for *every* program, including
   `HelloWorld`. That is the crate's real weight problem, and the
   `synthetic-jdk` feature is entirely orthogonal to it.

### Modules with **zero** real-JDK reachability

The complete synthetic-only residue — every one of these is reached solely from
`register_builtins` (`lib.rs:22419–22483`):

| Module | LoC |
|---|---:|
| `jdk25_concurrency.rs` | 4,962 |
| `t3_impl.rs` (JNDI, StAX, scripting, i18n, tooling, structured concurrency) | 2,257 |
| `phases_late/collections.rs` | 1,761 |
| `classfile_api.rs` | 1,680 |
| `jdk25_patterns.rs` | 1,476 |
| `phases_late/io_streams.rs` | 734 |
| `deprecated_verify.rs` | 503 |
| `phases_late/management.rs` | 422 |
| `bc_newhope_tables.rs` (pure data, no fns) | 286 |
| **total** | **14,081 (2.7 %)** |

### Conditionally-registered bucket

Small, and mostly *not* conditional in the shipped build:

| Gate | Modules | Default state |
|---|---|---|
| `experimental-jmx` | `jmx.rs` (5,678) | **ON** — in `vm/Cargo.toml` default features |
| `experimental-serialization` | `serialization.rs` (7,411) | **ON** |
| `experimental-aot` | `aot.rs`, `aot_pipeline.rs`, `cds.rs` (~6,100) | **ON** |
| `experimental-tls` | — | no-op alias; TLS is always compiled |
| `gpu-offload` | `craton_gpu.rs` (2,391) | OFF |
| `app-stubs` | small slices of `apps_h2.rs` | OFF |
| `legacy-synthetic-crypto` | small slices of `phases_early.rs` | OFF |
| `synthetic-quarkus-arc` | `quarkus_arc.rs` opt-in path | OFF (also env-gated) |

So of the eight declared features, only four are genuinely off by default, and
they gate ~2.4 kLoC of *registration*, not of compilation.

---

## 3. Do `Bridge` tags make most of the crate permanently live? **No — the tags are inert here.**

The `native-io` audit's framing ("almost every registration is `Bridge`, and
Bridge is never suppressed") does not transfer, for a reason that makes the
question moot: **on the default build nothing is suppressed by category at all.**

There are three category-driven suppression mechanisms. Their default states:

| Mechanism | Where | Default (real-JDK) |
|---|---|---|
| `drop_synthetic_stubs` — drops **every** `SyntheticStub` registration | `native-api/src/registry.rs:4071` | **OFF.** Constructed from `CRATONVM_NO_STUBS` (`registry.rs:3979`); `vm_init.rs:1504` explicitly declines to force it on |
| `drop_real_layout_synthetic` — drops named layout-incompatible stubs | `registry.rs:4244–4340` | **ON**, but it is a hand-curated allow-list of ~6 classes (`StringReader`, `StringJoiner`, `EnumSet`, …) |
| runtime yield-to-bytecode for `SyntheticStub` | `interpreter.rs:30405` → `real_protected_stub_class` (`:30438`) | **ON**, but a hardcoded list of **9 classes** plus whatever `CRATONVM_REAL` names |

Consequence: a `SyntheticStub`-tagged native in `native-builtins` is, on the
default build, just as live as a `Bridge`-tagged one — except for ~15 named
classes. The tag is documentation and a lever for `CRATONVM_NO_STUBS`
differential testing; it is not a gate.

For completeness, the tag distribution differs sharply from `native-io`'s.
Approximate ambient-category simulation over all 11,976 `register(` sites in
`native-builtins/src` (authoritative number requires `--dump-native-registry`):

| Tag | Sites | Share |
|---|---:|---:|
| `Bridge` | ~5,629 | 47 % |
| `SyntheticStub` | ~3,851 | 32 % |
| `Intrinsic` | ~2,496 | 21 % |

Roughly a third of this crate's registrations carry the *default* tag
(`current_category` starts at `SyntheticStub`, `registry.rs:3975`), which is
precisely the ambient-inheritance hazard the `d8092acb` post-mortem describes.

**Answer to the framing question: no.** What keeps `native-builtins` permanently
live is not the `Bridge` tag — it is that `register_essential_natives` calls 183
in-crate sub-registrars spanning 91 modules.

---

## 4. Sizing the `native-essentials` split

§7.4 step 2 says: *"The real-JDK arm of `vm_init.rs` calls a small, enumerable set
of `register_*` functions (essentials, concurrent, stamped-lock, JMX,
`Function$Identity`, SLF4J binder, the LBQ `drainTo` bridge). Move exactly those
into a `native-essentials` crate."*

The set is enumerable. It is not small.

| Split granularity | Moves to `native-essentials` | Stays behind `synthetic-jdk` |
|---|---:|---:|
| **one hop** from `register_essential_natives` + `vm_init` real arm | **91 modules, 357,075 LoC** | 76 modules, 164 kLoC |
| **transitive closure** (what actually has to compile) | **158 modules, 507,296 LoC (97.3 %)** | 8 modules + 1 data file, **14,081 LoC (2.7 %)** |

The transitive figure is the one that governs build time and binary size, because
Cargo's unit of compilation is the crate: if `native-essentials` contains
`lib.rs`, `phases_early.rs`, `phases_late/**`, `lang_class.rs`, `lang_invoke.rs`
and `classloader.rs`, it contains the modules those reference, and the closure
runs to 97 % of the crate. **A `native-essentials` crate would be
`native-builtins` minus 2.7 %.**

### Concrete module list

`native-synthetic` (the residue that can move *out* of the default build) —
9 files, 14,081 LoC:

```
native-builtins/src/jdk25_concurrency.rs
native-builtins/src/jdk25_patterns.rs
native-builtins/src/classfile_api.rs
native-builtins/src/t3_impl.rs
native-builtins/src/deprecated_verify.rs
native-builtins/src/phases_late/collections.rs
native-builtins/src/phases_late/io_streams.rs
native-builtins/src/phases_late/management.rs
native-builtins/src/bc_newhope_tables.rs      (data only; check bc_newhope.rs first)
```

Dependency edges: all nine are leaf-ward. Their only in-crate callers are the
`register_builtins` body (`lib.rs:22419–22483`) and each other; nothing in the
real-JDK closure references them. So the edge set is
`native-synthetic → native-api, types, native-builtins(shared helpers)` — and
that last edge is the problem: they call `lib.rs` helpers (`obj_arg`,
`alloc_concurrent_synthetic`, …), so extracting them requires either duplicating
those helpers or promoting them into a third crate. For 14 kLoC that is not worth
it. **Recommendation: leave the crate whole; delete or feature-gate the nine
modules in place if the goal is dead weight, and pursue the app-shim split below
if the goal is build time or binary size.**

### The split that would actually pay

The 43.5 % figure is not driven by synthetic-JDK debt. It is driven by
**third-party application shims that the default build registers unconditionally
for every program**:

| Candidate crate | Modules | LoC | Registered for |
|---|---|---:|---|
| `native-appshim-jboss` | `jboss_*`, `wildfly_*`, `xnio_*`, `jca/**`, `agroal_pool`, `ironjacamar_pool`, `infinispan_local` | ~48,000 | WildFly only |
| `native-appshim-web` | `servlet`, `http2`, `http_client`, `http_url_connection`, `cglib_enhancer`, `spring_startup_bootstrap`, `test_frameworks` | ~30,000 | Spring/Tomcat only |
| `native-appshim-data` | `apps_h2`, `jdbc`, `orm_hibernate`, `antlr_intrinsics`, `lucene_es`, `xml_xerces` | ~20,000 | H2/Hibernate/ES only |
| `native-crypto-bc` | `bc_*`, `phases_late/bouncycastle.rs` | ~22,000 | BouncyCastle only |

That is ~120 kLoC (23 % of `src/`) with a clean seam (each is called from exactly
one place in `register_essential_natives`) and no synthetic/real entanglement.
**This is the split worth specifying.** It is orthogonal to `synthetic-jdk` and
does not touch the `d8092acb` hazard surface at all, because it removes whole
`register_*` calls rather than reclassifying individual registrations.

*(No `Cargo.toml` was modified: this session does not own them.)*

---

## 5. How this avoids the `d8092acb` regression

`d8092acb` ("fix-tests-real-jdk-contracts", 2026-07-14) added
`native_methods.set_drop_synthetic_stubs(true)` to the real-JDK arm and was
reverted the same day. The post-mortem lives at `vm/src/vm/vm_init.rs:1503–1509`
(the `#[cfg(not(feature = "synthetic-jdk"))]` arm) and `:1057–1093` (the
`synthetic-jdk`-build real arm). Its failure mode, precisely:

> unconditionally dropping ALL SyntheticStub-tagged natives in real-JDK mode …
> several `register_*` clusters that are tagged SyntheticStub are actually needed
> as permanent bridges in BOTH modes (no working real-bytecode fallback exists).

Confirmed casualties: the entire `java.lang.management`/JMX surface
(`ManagementFactory.getPlatformMBeanServer()` NPE deep inside real
`javax.management` bytecode) and `java.util.function.Function$Identity` (a
VM-internal stand-in with no real bytecode at all → `UnsatisfiedLinkError`).
Later casualties of the same family: `register_properties_sidetable`
(`InternalError: null property: java.home`) and `CopyOnWriteArrayList`'s mutators
(`"this.lock is null"`).

The root cause is **not** "we dropped too much". It is that
`NativeKind` is an *ambient* property — `current_category` persists across
`set_category` calls and is inherited by whole `register_*` functions that never
set it — so the tag is not a reliable predicate for "is this a fake". Whole
functions' worth of genuine bridges inherited `SyntheticStub` by accident. The
registry's own source says so at `registry.rs:4077` and again at `:4380–4399`
("category-matching is otherwise inherently fragile — any future `set_category`
reshuffle …").

This proposal avoids it in three ways:

1. **It never reclassifies or drops anything at registration time.** The split
   moves whole modules between crates. A module either compiles into the default
   build and registers exactly as it does today, or it does not compile in at
   all. There is no third state where a registration is silently absent while its
   call site still expects it.
2. **Its partition is derived from the call graph, not from `NativeKind`.** The
   nine synthetic-only modules above are identified by "no path from
   `register_essential_natives` / the `vm_init` real arm / any `vm`-side entry
   point reaches them", which is a property of the code, not of an inherited
   ambient tag. JMX (`jmx.rs`), `properties_sidetable.rs` and
   `Function$Identity` — the exact `d8092acb` casualties — all sit firmly in the
   **real-reachable** bucket under this rule, which is the check that the tag
   failed.
3. **It does not delete before the census.** §7.4 step 3 stands unchanged: run
   `--dump-native-registry` plus `--dump-missing-natives-grouped` across
   H2/Spring/WildFly/Tomcat in real-JDK mode before removing any of the nine.
   This document is the *static* upper bound on what is dead; the census is the
   *dynamic* confirmation, and only their intersection is safe to remove.

Concretely: had this method been applied on 2026-07-14, `jmx.rs` would have been
classified real-reachable (`vm_init.rs`'s `#[cfg(not(feature = "synthetic-jdk"))]`
arm calls into it at ten distinct sites — `:2021` and `:2055–2068`) and would
never have been a deletion candidate,
regardless of its `SyntheticStub` tag.

---

## 6. Fixes landed in this session

All in files this session owns (`native-builtins/src/**`). No behaviour is
gated behind a new env var; no default was flipped off.

### Uncached `std::env::var` on per-call paths

Three debug switches were probed via `getenv` on every invocation. `getenv` takes
the process environ lock and scans `environ` linearly — the same cost that
`lib.rs:38892`'s `nbflags()` doc records as 130 M calls per CratonBench
`hashmap` run before `c258662e4`. All three now latch into a `OnceLock<bool>` at
first use, matching the established
`security_manager::dbg_dopriv_enabled` / `lang_reflect::dbg_method_invoke_box_enabled`
convention (the switch must be set before first use to take effect).

| Site | Frequency | Fix |
|---|---|---|
| `tests_extracted.rs` — `ByteArrayOutputStream.write(int)` (`CRATON_BAOS_DBG`) | **once per byte** written to any BAOS: DER encoding, serialization, `PrintStream`, every `toByteArray` pipeline | `baos_dbg_enabled()` |
| `apps_h2.rs` — `h2_parser_read` (`CRATONVM_DBG_H2PARSERREAD`) | once per SQL token; hundreds per statement | `h2_parser_read_dbg_enabled()` |
| `spring_startup_bootstrap.rs` — `walk_imports_recursive` (`CCPP_DBG`) | once per absent `@Import` target; hundreds per Spring Boot startup. Also used `env::var`, allocating a `String` per call | `ccpp_dbg_enabled()` |

`native-builtins/src/lib.rs` — `resolve_real_hostname()` now latches its result.
Step 3 of its resolution order **forks and execs `hostname(1)`**, and on Linux
`HOSTNAME` is frequently not exported to non-interactive shells, so steps 1–2
miss and every `InetAddress.getLocalHost()` / `NetworkInterface.getNetworkInterfaces()`
call paid a full process spawn. The probing logic moved to
`resolve_real_hostname_uncached()` so it stays directly testable.

### A latent correctness bug found alongside it

`net_phase_e.rs::hostname_string()` was a third, divergent copy of hostname
resolution: it checked `COMPUTERNAME` **before** `HOSTNAME` (the opposite
precedence to `resolve_real_hostname`) and had no `hostname(1)` fallback. Both
functions back a registration of the *same* method,
`java.net.InetAddress.getLocalHost()` — `net_phase_e.rs:4350` and
`net_uri_inet.rs:1265` — so on a Linux host without an exported `HOSTNAME`, which
answer a program saw (`"localhost"` vs. the real machine name) depended only on
which registrar ran last. `hostname_string()` now delegates to
`crate::resolve_real_hostname()`, which is what that function's own doc comment
already claimed ("reused by `getLocalHost` and
`NetworkInterface.getNetworkInterfaces` so both surface the same name").

### Test coverage added

* `apps_h2.rs::h2_parser_read_dbg_tests` — flag is latched, matches the
  environment at first use, and is off in a clean environment.
* `tests_extracted.rs::baos_dbg_flag_tests` — same, plus an explicit guard that
  the per-byte `eprintln!` can never become default-on.
* `spring_startup_bootstrap.rs::ccpp_dbg_flag_tests` — same.
* `net_uri_inet.rs::resolve_real_hostname_is_cached_and_matches_uncached_probe`
  — the cache is stable across calls and does not change the resolved value.
* `net_phase_e.rs::hostname_string_tests` — the two `getLocalHost()`
  implementations agree, and the result is non-empty and stable.

Not built or tested (concurrent builds OOM this host). Every edited file was
parse-checked with `rustfmt --edition 2021 --check` on a scratch copy; CRLF line
endings preserved in place.

---

## 7. Method and caveats

* The call graph is **static and name-based**: a `fn` is an edge target if its
  bare name appears in a caller's body as a call or after `::`. Same-named
  functions in different modules collapse, which over-approximates reachability —
  i.e. it makes the *real* bucket too big and the deletion list too small. That
  is the safe direction for a "what can we remove" question, and it is why §4's
  synthetic-only list should still be confirmed by census before deletion.
* Closures passed inline to `registry.register(...)` are the bulk of the actual
  native implementations. They are attributed to the enclosing `register_*`
  function, which is correct for reachability but means "LoC per module" is
  dominated by a handful of very large registrars.
* `#[cfg(feature = "synthetic-jdk")]` regions are detected by brace-matching from
  the attribute; `#[cfg(any(feature = "synthetic-jdk", …))]` counts as synthetic.
* The category tally in §3 simulates `set_category`/`with_category` linearly and
  guesses at `set_category(__prev_*)` restore markers. Treat it as ±10 %; the
  authoritative number is `--dump-native-registry`.
* Roots: 171 symbols, 162 resolved in-crate. The 9 unresolved are `register_*`
  functions owned by `native-io` / `native-collections` / `native-awt`.
