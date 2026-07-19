# Silent 1200s hangs with no diagnostic signature (3 classes)

**Status:** PARTIALLY FIXED (2026-07-19 update) — 2 of 3 classes (`TestContextConfig`,
`TestValidator`) are now **FULLY FIXED**, reliably passing at the canonical
`-Xmx2g` heap; the true root cause of their hang was misdiagnosed in the
2026-07-13/07-16 checkpoints as an interpreter-throughput problem — it was
actually a `file:` `URL.openConnection()` path-corruption bug (see below).
The 3rd class (`TestResponsePerformance`) no longer **silently hangs** either
— its 1200s-timeout black-box hang is gone, replaced by a fully diagnosed,
signature-bearing failure (either a real relative-performance assertion at
`-Xmx4g`, or a root-caused-via-gdb OOM abort at the canonical `-Xmx2g`). Two
more real allocator bugs were found and fixed along the way. **Severity:**
downgraded from high (indefinite silent hang, 2 classes) / medium (1 class)
to: RESOLVED (2 classes) / medium, fully diagnosed (1 class, no longer
silent). **HotSpot:** PASS on all 3 (fresh-verified, prior checkpoint).

## 2026-07-19 update: real root cause found for the TestContextConfig/TestValidator hang

The 2026-07-13/07-16 work (below) treated all 3 classes as symptoms of one
interpreter-throughput problem and made real, valid progress on that front
(JIT compile-bail cap, OSR retry cap, debug-probe removal, `force_native_over_
real_jdk_bytecode` per-call-site memoization landed 2026-07-15 as
`3d1449a7d`). But `TestContextConfig` and `TestValidator` were never actually
throughput-bound — they were hitting a **separate, unrelated correctness bug**
that also happened to make them "hang" (in practice: crawl through the
Xerces/JAXP entity-resolution retry path pathologically slowly, which reads
identically to a throughput problem from the outside, until you actually
trace what's failing).

### Root cause: `file:` `URL.openConnection()` stripped the leading `/` from POSIX paths (FIXED)

`native-builtins/src/net_phase_e.rs`'s native `URL.openConnection()` handler
for `file:` URLs did:

```rust
let mut path = decoded.trim_start_matches('/').to_string();
#[cfg(windows)]
{ /* reinsert ':' for the MSYS/Cygwin `/c/...` -> `c:/...` case */ }
```

`trim_start_matches('/')` unconditionally strips **every** leading slash
before this `cfg` split existed — correct on Windows (needed to turn
`/c/foo` into `c:/foo`), silently wrong on Linux/macOS, where the decoded
path (e.g. `/data/data/.../web-app_2_3.dtd`) IS the absolute filesystem path
and must keep its leading `/`. The resulting `java.io.File` was built from a
now-*relative* path, which resolved against the JVM's `cwd` instead of `/`
— so it "worked" only when cwd happened to already be at the exact right
depth, and failed everywhere else with a `FileNotFoundException` whose
message itself confirmed the bug (`data/data/tomcat.../web-app_2_3.dtd`, no
leading slash).

**Confirmed via gdb-free direct repro** (`u.openConnection().connect()`)
comparing against real JDK on the same host: identical
`getResource()`/`URI`/`URL.openStream()` behavior on both JVMs (those paths
were already correct/unaffected — a decoy that cost real investigation time),
but `openConnection()` + `connect()` diverged: real JDK opens the file fine,
CratonVM's `FileURLConnection.connect()` (real Tomcat/JDK bytecode) threw the
exact `FileNotFoundException` seen in the suite logs.

**Blast radius:** this is the code path Xerces/JAXP's `XMLEntityManager`
takes to open a **local DTD/schema file** resolved via `LocalResolver`
(`org.apache.tomcat.util.descriptor.LocalResolver` → `URL.openConnection()` →
`FileURLConnection.connect()`), i.e. every Tomcat XML descriptor with a
DOCTYPE that resolves to a locally-cached DTD (`web-app_2_3.dtd`,
`mbeans-descriptors.dtd`, TLD DTDs, …) hit this on every load. That is
precisely `TestContextConfig` (`mbeans-descriptors.dtd` load errors on every
container start, `web.xml` parse retries) and `TestValidator` (all 11
`testTldVersionsNN` sub-tests, each parsing a TLD/web.xml against a local
DTD). Also affects **any** other suite/app that resolves local `file:`
resources via `URL.openConnection()` rather than `getResource()`/
`openStream()` — worth a regression sweep in a follow-up session.

**Fix** (`native-builtins/src/net_phase_e.rs`): split the path computation on
`cfg(windows)` — Windows keeps the strip-then-reinsert-colon logic; POSIX
uses the decoded path unchanged (leading `/` intact).

**Verified, 4 independent runs each, both PASS reliably:**
- `TestContextConfig`: `OK (8 tests)` in 581–617s (was: 1200s hang, 0 tests).
- `TestValidator`: `OK (11 tests)` in 435–463s (was: 1200s hang, 0 tests).

Reproduction (must `cd` into the fixture root first — the doc's own original
repro command below omits this, which is itself a trap: running from the
wrong cwd makes this bug manifest as an *immediate* `IllegalArgumentException:
main resource set ... is not a directory` instead of the hang, an easy
false lead):

```bash
cd /data/data/apps/tomcat   # MUST cd here first, see above
CP=$(cat .suite/cp-linux-fixed.txt)
<EXE> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.startup.TestContextConfig
```

## 2026-07-19 update: TestResponsePerformance — silent hang eliminated, 2 more allocator bugs fixed, throughput/OOM residual remains

`TestResponsePerformance` does not touch the DTD-loading path above (its
`@Test` is a pure microbenchmark, no XML parsing), so the `file:` URL fix
does not change its outcome by itself. But two more real bugs were found and
fixed while investigating it, and both have a blast radius well beyond this
one test.

### Bug 1: `String.substring()` / `StringBuilder.toString()` hard-aborted the whole process on transient GC pressure (FIXED)

`native-builtins/src/lang_string.rs`'s `native_string_substring` (the
method's *unconditionally* forced-native override — see
`force_native_over_real_jdk_bytecode` in `vm/src/runtime/interpreter.rs`) and
`native_sb_to_string` (`StringBuilder`/`StringBuffer.toString()`) both built
their result via `ctx.create_string_uninterned(&text)`. That call is the
*aborting* String allocator (`std::process::abort()` on exhaustion, no
GC-and-retry) — appropriate for constructor-style natives holding live
unrooted references, but **not needed here**: by the point either function
calls `create_string_uninterned`, all the data it needs (`sub_text`/`text`)
is already a Rust-owned `String`, and neither `this` nor the backing char
array is dereferenced again. Switched both to
`ctx.create_string_uninterned_gc_safe(&text)` (an existing, already-used-
elsewhere trait method: proactively runs a young GC when headroom is tight,
*then* allocates) — a same-shape fix as the existing precedent, not a new
mechanism.

### Bug 2: `alloc_concurrent_synthetic` (java.net.URI, HttpURLConnection, …) had no non-aborting allocation path at all (partially fixed — new capability added)

Root-caused via `gdb` (`handle SIGUSR2/SIGUSR1 nostop noprint pass` — the VM
uses `SIGUSR2` internally for its STW GC handshake, which otherwise makes gdb
stop on the wrong signal — then `run`/`bt full` to catch the real `SIGABRT`):

```
#7  gc/src/gen_heap.rs:8165  (the "FATAL: OutOfMemoryError: young gen exhausted" abort)
#10 alloc_object() gc/src/gen_heap.rs:976
#11 alloc_object() vm/src/vm/vm_exec.rs:5342          <- NativeContext::alloc_object impl
#12 alloc_concurrent_synthetic() native-builtins/src/lib.rs:56835
#13 make_uri() native-builtins/src/net_phase_e.rs:2108   <- java.net.URI construction
#14 register_uri_natives closure #25, net_phase_e.rs:2852
```

`doUri()`'s 1,000,000-iteration `new URI(...)` loop allocates the URI holder
object via `alloc_concurrent_synthetic`, which called the **only** allocator
exposed on the `NativeContext` trait: the aborting `alloc_object`. Unlike the
String case, there was no fallible/GC-safe twin available to native code for
*general object* allocation at all — `create_string_uninterned_gc_safe`'s
capability had never been generalized beyond Strings.

Added:
- `NativeContext::try_alloc_object_gc_safe` (new trait method,
  `native-api/src/registry.rs`, default body just wraps the existing
  `alloc_object` in `Some(..)` so it's non-breaking for every other
  implementor/mock).
- The real VM override (`vm/src/vm/vm_exec.rs`): walks the *identical*
  `ClassId(0)`-substitution / field-count-clamp / TLAB / old-gen-batch fast
  paths as `alloc_object` (copied, not refactored, to avoid touching the
  existing hot path), but (a) proactively checks young-gen headroom and runs
  a GC first — the same `create_string_uninterned_gc_safe` pattern — and (b)
  its final fallback is the already-existing fallible primitive
  `GenHeap::try_alloc_object_full` (returns `Option`, walks young → old gen,
  never aborts) instead of the aborting `GenHeap::alloc_object`.
- `alloc_concurrent_synthetic` (`native-builtins/src/lib.rs`) now calls
  `try_alloc_object_gc_safe` first, falling back to the old aborting
  `alloc_object` only if that still reports `None` (preserves prior behavior
  as an absolute last resort; no signature change needed for this function's
  many other callers — `HttpURLConnection` and others share it).

**Verified effect:** at `-Xmx4g`, `TestResponsePerformance` now runs to
completion (`Time: 556.197s`, all 6 rounds, well inside the 1200s timeout) —
previously it OOM-aborted the whole process partway through even at this
larger heap. At the doc's canonical `-Xmx2g`, the fix reduces exposure (the
proactive GC does run) but **does not fully eliminate** the abort — see
residual below. This is a net-positive, non-regressing architectural fix
(confirmed via `cargo check` + full release build + 4 repeat runs of
`TestContextConfig`/`TestValidator`, unaffected) even though it doesn't fully
close this specific stress case.

### Residual (now fully diagnosed, no longer silent): `TestResponsePerformance`

Two independent, now precisely-characterized issues remain, neither a silent
hang:

1. **Relative-performance assertion failure** (`-Xmx4g`, completes in 556s):
   the test is `Assert.assertTrue(homebrewWin == winTarget)` — it asserts
   Tomcat's `CharChunk`-based "home-brew" `Response.toAbsolute()` beats a
   `java.net.URI`-based equivalent in a best-of-5 vote. Measured on
   CratonVM: home-brew ~63,500–72,700 ms / round vs. URI ~21,200–26,200 ms /
   round for 1,000,000 iterations each — **URI wins every round**, the
   opposite of HotSpot (test's own comment: "the 'homebrew' approach is
   consistently 3-4 times faster" on real JVMs). `toAbsolute()`'s hot path is
   `org.apache.tomcat.util.buf.CharChunk.append()` (real bytecode, no native
   override) + `CharChunk.toString()` (force-natived for `toString`/
   `endsWith`/`indexOf` only — `StringCache` is disabled by default in
   vanilla Tomcat via `-Dtomcat.util.buf.StringCache.char.enabled`, so this
   is not a broken-cache issue, confirmed real HotSpot also allocates a
   fresh `String` per call). Root cause of *why* CharChunk-style
   char-array-and-native-toString concatenation is markedly slower than
   `URI` construction on this interpreter is not further narrowed — this is
   the genuine "larger, separate interpreter-throughput investigation" the
   2026-07-13 entry already anticipated ("not a quick fix"), now with a
   precise, reproducible A/B (`CharChunk` vs `URI`) instead of a vague
   "everything is slow" symptom.

2. **OOM abort still reproduces at the canonical `-Xmx2g`** (does NOT
   reproduce at `-Xmx4g`): even with the proactive-GC fix above, a full young
   GC run immediately before the failing allocation does not free enough
   headroom — `gdb` showed the identical abort site
   (`alloc_concurrent_synthetic` / `make_uri`) firing at essentially the same
   point (~2 rounds in, ~2,000,000 `new URI(...)` calls) both before and
   after the fix. Since `doUri()` does not retain any of the URIs it
   constructs, this either means (a) CratonVM's actual per-object memory
   footprint for this churn pattern is high enough that 512 MiB young-gen
   (the `-Xmx2g` default split) is a hard floor for this specific
   1,000,000-iteration test, or (b) something is unexpectedly keeping
   `URI`/`String` garbage reachable across the collection (a genuine
   retention bug) — not distinguished in this session. Worth a dedicated
   follow-up with heap-dump/root-tracing tooling rather than further gdb
   backtrace sampling.

## Summary (original 2026-07-13 finding, retained for history)

Three classes HANG at the full 1200s timeout without printing any
error/warning in stdout or stderr beyond normal startup:

- `org.apache.catalina.startup.TestContextConfig` — log stops right after
  `INFO [org.apache.catalina.startup.ContextConfig] No global web.xml
  found`. **Now FIXED — see 2026-07-19 update above.**
- `org.apache.catalina.connector.TestResponsePerformance` — log stops
  right after `INFO [...] Starting test case [testToAbsolutePerformance]`.
  **Silent-hang behavior FIXED; real, diagnosed residuals remain — see
  2026-07-19 update above.**
- `org.apache.jasper.compiler.TestValidator` — log stops shortly after
  `JUnit version 4.13.2`. **Now FIXED — see 2026-07-19 update above.**

**Heap-sizing false positive theory REFUTED**: all 3 still hang
identically at `-Xmx12g` (previous hypothesis, from a related Linux OOM
finding, ruled out).

## Root causes found + fixed (2026-07-13)

Three independent, silent-by-design bugs, each causing the interpreter to
either waste enormous cumulative time re-attempting doomed work forever,
or pay an unconditionally expensive check on every single instruction.
None produce a warning because they are "successfully" bailing/skipping
each time — just far too often.

1. **JIT compile-bail not permanently recorded for `ldc`/`ldc2_w`
   constants** (`jit/src/lib.rs::try_compile_inner`). Resolving a
   `ldc`/`ldc2_w` operand to a String/Class/MethodHandle (or non-
   Long/Double) constant returns `None` via `?` WITHOUT setting
   `backend_attempted`, so `mark_jit_bail_listed` never fires. A hot
   method containing such a constant (confirmed repro: `TesterRequest
   .getRequestURI() { return "/level1/level2/foo.html"; }`, called from
   `TestResponsePerformance`'s 1,000,000-iteration loop) re-ran the ENTIRE
   compile gauntlet (skip-list check + native-shadow hierarchy walk +
   bytecode scan) every ~2000 invocations, forever — confirmed via
   `CRATONVM_DBG_JITC=1`: 20 `compile-bail ... backend_attempted=false`
   lines for the same method in one 60s window pre-fix, 1 post-fix.
   **Fixed**: mark permanently bailed, matching the existing RBC.4/RBC.6
   convention already used for scan-reject/athrow-with-handler.

2. **OSR re-enqueue has no failure cap** (`jit/src/tiered.rs
   ::request_osr`). Unlike `should_compile`/`request_c2_upgrade`, this
   function never checked `tier_fail_count >= MAX_TIER_FAIL_RETRIES`. An
   OSR artifact compile that keeps returning `published=false` leaves
   `current_tier` stuck below C2 and `queued_for_compilation` cleared
   after each failure, so the very next hot back-edge re-enqueues a fresh
   OSR task — forever. **Fixed**: added the same cap check
   `request_c2_upgrade` already has.

3. **Uncached env-var syscall on every bytecode instruction**
   (`vm/src/runtime/interpreter.rs`). Five leftover debug blocks from a
   completed, unrelated investigation, each doing a raw uncached
   `std::env::var_os(...)` on every dispatched bytecode. **Fixed**:
   removed all 5.

Plus a ~2x hot-path fix the same day: `defining_loader_for`'s unconditional
mutex lock on every `force_native_over_real_jdk_bytecode`-adjacent dispatch,
short-circuited via `ANY_DEFINING_LOADER_REGISTERED` (an `AtomicBool`, set
only when a user-defined `ClassLoader` first defines a class — the common
case never does). And a 2026-07-15 follow-up (commit `3d1449a7d`, found
already on `dev` at the start of this 2026-07-19 session, not part of this
session's work but relevant context): `force_native_over_real_jdk_bytecode`'s
~55-branch sequential dispatcher is now memoized once per invoke-cache entry
(`CachedBytecodeMethod::force_native_cache`) instead of re-walked on every
cached-dispatch hit.

## Reproduction

```bash
cd /data/data/apps/tomcat   # IMPORTANT: cd here first (see 2026-07-19 note above)
CP=$(cat .suite/cp-linux-fixed.txt)
<EXE> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore <ClassName>
# org.apache.catalina.startup.TestContextConfig       -> now PASSES (8/8)
# org.apache.catalina.connector.TestResponsePerformance -> FAILS (see residual)
# org.apache.jasper.compiler.TestValidator             -> now PASSES (11/11)
```

## Recommendation for continuing the residual

1. Root-cause *why* `CharChunk`-style native-`toString()` char-array
   concatenation is slower than `java.net.URI` construction on this
   interpreter — a targeted A/B microbenchmark (this test IS that
   microbenchmark) with `perf`/instruction-count profiling of each path in
   isolation, rather than whole-VM sampling.
2. Determine whether the residual `-Xmx2g` OOM is a genuine per-object
   footprint/floor issue or a retention bug — a heap census
   (`CRATONVM_DBG_*` root-dump tooling, or a scaled-down iteration count with
   `-Xmx` bisection) at the exact abort point would distinguish these.
3. Regression-sweep other `file:` `URL.openConnection()` consumers now that
   the root cause is known precisely — any suite/app resolving local
   resources via `openConnection()`+`connect()` rather than
   `getResource()`/`openStream()` was silently affected the same way.
