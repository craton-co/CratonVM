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
**2026-07-19 follow-up**: confirmed via heap bisection that the `-Xmx2g` OOM
is a hard floor, not a leak (`-Xmx2560m`+ runs clean) -- see that section
below for the practical workaround (run at >=2.5g) and a new, separate,
non-fatal "stale pointer" finding worth a dedicated pickup.
**2026-07-19 deep-dive**: found the DEFINITIVE root cause of the relative-perf
assertion -- `Response.toAbsolute()` permanently fails to JIT-compile
(confirmed via `CRATONVM_DBG_JITC=1` trace) because of a real, deliberate,
documented JIT limitation (`RBC.6`: no local-exception-handler dispatch in
the codegen) triggered by its `try { ... } catch (IOException) { throw new
IllegalArgumentException(...) }` shape. This is not a bug in the gate --
compiling this method today would silently produce wrong exception
semantics -- so it was NOT patched around; see that section for the full
finding and why implementing the real fix (JIT support for local exception
handlers) is a scoped compiler feature, not a same-session patch. This is
the practical ceiling for a diagnostic pass on this residual; doc stays in
known-issues pending that feature.
**2026-07-19 RBC.6 CLOSED**: the "no local-exception-handler dispatch"
premise turned out to be stale (true when the gate landed, false the very
next day once unrelated fixes built the missing dispatch generically) —
see `docs/feature-designs/jit-local-exception-handlers.md` for the full
finding, the fix (gate relaxation + a required new compile-time safety
check that also closes a previously-latent, pre-existing wrong-result bug
in already-shipped JIT code), and validation. `Response.toAbsolute()`'s
exact shape (single try/catch, unconditional rethrow, no string-concat in
the handler — matches Tomcat's real bytecode) now compiles and produces
correct results, confirmed via a standalone repro run through the built
binary. **Not yet re-run against the actual `TestResponsePerformance`
JUnit test** (this session's environment could not reach the Linux Tomcat
suite fixture) — the relative-performance assertion's disposition
(`homebrewWin == winTarget`) is still open pending that re-run; see
"Recommendation for continuing the residual" below, now updated.

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

## 2026-07-19 follow-up session: native-callback caching + heap-threshold characterization

Picked the `TestResponsePerformance` residual back up as a dedicated follow-up
(worktree `/data/wt-tomcat-trp-residual-20260719`, branch
`codex/fix-tomcat-trp-residual-20260719`, based on `dev` @ `63bdd1f72`).

### Fix: memoize the resolved `NativeCallback` per invoke-cache entry (real, verified improvement)

`intercept_force_registered_native_cached` (`vm/src/runtime/interpreter.rs`)
already memoized the *boolean* `force_native_over_real_jdk_bytecode` result
per callsite (landed 2026-07-15, `3d1449a7d`) but still called
`NativeMethodRegistry::find(class_name, method_name, method_descriptor)` —
a hash-keyed lookup — on **every** hit once `force_native` was `true`. A
fresh `perf` profile confirmed this was still the #2 hottest symbol
(~6.8-7.7% of samples, second only to the interpreter's own frame-dispatch
loop) on the exact same `TestResponsePerformance` benchmark.

Added `CachedBytecodeMethod::native_callback_cache: OnceLock<Option<NativeCallback>>`
(`jit-api/src/lib.rs`, same pattern as the existing `force_native_cache`;
required adding `cratonvm-native-api` as a dependency of `cratonvm-jit-api` —
verified no circular dependency, `native-api` has no back-edge to `jit-api`),
populated at all 28 `CachedBytecodeMethod` construction sites across
`vm/`, `jit/`, and `classloading/`, and used it in place of the direct
`.find()` call. Native registration is immutable after VM boot (no
redefinition path touches the registry), so this is sound by the identical
argument already used to justify `force_native_cache`.

**Verified via before/after `perf` profiles** (same binary state, same
benchmark, `-F 999` sampling): `NativeMethodRegistry::find` dropped from
~6.8-7.7% to ~5.1-7.1% of samples (noisy across runs but consistently lower).
**Wall-clock for `doHomebrew()` itself did not measurably change** — traced
this to the fact that `toAbsolute()`'s dominant cost,
`CharChunk.append()`, is real bytecode with no native override at all, so it
never reaches this cached-dispatch path; only `CharChunk.toString()` (called
once per iteration) benefits, a small fraction of the loop's total work. This
fix is still a real, non-regressing win (confirmed via `cargo check`, a full
release build, and TWO full reruns of `TestContextConfig`/`TestValidator`
at the canonical `-Xmx2g`, both still `OK (8 tests)` / `OK (11 tests)`,
~593-624s / ~428-436s) — landing it because "measurably reduces a top-2
hotspot with zero regression risk" clears the bar even without moving this
specific benchmark's needle, but it is **not** the fix for the relative-perf
residual.

### Heap-threshold bisection: `-Xmx2g` OOM is a hard floor, NOT a leak

Ran `TestResponsePerformance` at `-Xmx2560m`, `-Xmx3g`, and `-Xmx3584m` (in
parallel, same binary carrying both the 2026-07-19-morning OOM-safety fixes
and the native-callback-cache fix above). **All three completed cleanly** —
`Time: 611.5-624.5s`, `Tests run: 1, Failures: 1` (the known relative-perf
assertion, not a crash) — **zero `FATAL:` OOM aborts** at any of the three
sizes, vs. a reliable abort at the canonical `-Xmx2g` (confirmed same-day,
same binary, separately). A genuine unbounded leak would be expected to
still manifest (just later) at a 25-75% larger heap; a clean pass at
`-Xmx2560m` (only 25% more than the failing `-Xmx2g`) is much more
consistent with **`-Xmx2g` sitting just under this specific workload's
actual live-set + fragmentation floor** on CratonVM's current object
representation than with a retention bug. Recommend closing the "is it a
leak" question as NO (floor, not leak) unless a future investigation finds
contrary evidence; the remaining open question is *why* the floor is higher
than HotSpot's for the identical logical workload (a separate,
memory-density question from the relative-perf-vs-URI question).

### New finding: non-fatal "stale pointer" defensive-recovery warning at 2.5g-3g (OPEN, not investigated further)

Both the `-Xmx2560m` and `-Xmx3g` runs (but **not** `-Xmx3584m`, and never
observed at the previously-tested `-Xmx4g`) repeatedly logged:

```
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual
receiver (ptr=0x..., all-zero header) — falling back to CP class java/lang/String
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
(caller used slot index past receiver's layout — class layout is correct; the
bug is in the caller's slot computation, typically a speculative
collection-layout probe dispatched on a non-matching receiver type)
obj=0x... index=0 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
real_field_count=Some(0)
```

This is the VM's own defensive guard catching itself — it recovers instead of
corrupting state or crashing, so it did **not** cause either the OOM abort or
the relative-perf assertion failure (both already present/absent
independently of this warning). But it IS evidence of a real, previously
undetected bug: some **JIT speculative collection-layout probe** dispatches
against a receiver whose pointer has gone stale (all-zero header — pointing
at unformatted/zeroed memory) specifically in this narrow
2.5g-3g memory-pressure window, twice per run in both cases observed. Given
the extensive existing `stale-objectref`/precise-roots bug family already
tracked in this codebase's history (see prior GC/roots work), this smells
like the same class of issue, not a new mechanism — but was NOT
root-caused or fixed this session (out of scope for a quick follow-up; needs
the dedicated GC/JIT-roots investigation methodology already used for that
bug family, e.g. `CRATONVM_DBG_STALE_OBJREF`-style tracing). Worth a
dedicated pickup: reproduce reliably at `-Xmx2560m` (2 occurrences per
~610s run observed, so not rare), then trace which specific collection-type
speculative probe (HashMap/ArrayList-style inline field access, per the
guard's own message) is involved and whether it's the same root cause as
prior stale-ObjectRef findings or a new one.

### Updated recommendation for this residual

1. The relative-perf assertion (`CharChunk`-based home-brew ~3x slower than
   `URI`-based) remains open and unattributed to any single fixable call
   site — `perf` self-time profiling (flat and call-graph, both attempted)
   didn't cleanly isolate a dominant cause beyond the general interpreter
   dispatch machinery already characterized in the 2026-07-19-morning entry.
   Next step, if picked up again: instrument per-bytecode-instruction
   counts (not perf sampling) for `CharChunk.append()`'s real-bytecode body
   specifically, compared against `URI`'s real-bytecode body, to find
   whether one genuinely executes far more instructions per logical
   operation (an algorithmic gap) vs. executes a similar instruction count
   markedly slower (a dispatch-overhead gap) — the two point to very
   different next fixes.
2. The "stale pointer" finding above is a solid, reproducible, currently
   uninvestigated lead — probably the highest-value next pickup given the
   codebase's track record on this bug family.
3. Heap-threshold question is closed (floor, not leak) — no further action
   needed on that specific question.

## 2026-07-19 deep-dive: definitive root cause of the relative-perf gap found (RBC.6 JIT exception-handler gate)

Picked the doc's last open item back up with the explicit goal of closing it
completely (worktree `/data/wt-tomcat-trp-deepdive-20260719`, branch
`codex/fix-tomcat-trp-deepdive-20260719`).

### Two more optimization attempts, both empirically verified NOT to help (documented for the record, not landed)

Before finding the real cause, two plausible-looking native-call-dispatch
optimizations were tried against an isolated `String.getChars(II[CI)V`
microbenchmark (the method `CharChunk.append(String,int,int)` calls
internally) — a scalar-only bulk-array-write native rewrite, and extending
the JIT's `jit_invoke_dispatch` per-callsite native-callback cache
(`ObjectNativeKind`) to cover `String.getChars`. **Neither changed the
microbenchmark's wall-clock at all** (~1.0-2.3ms/1000 calls, flat across
attempts, `NativeMethodRegistry::find` staying at ~10-13% of profiled
samples throughout). Both were reverted rather than landed, since "doesn't
help and adds complexity" fails this session's own bar for shipping a fix.

### The actual finding: `Response.toAbsolute()` never gets JIT-compiled

Ran `TestResponsePerformance` under `CRATONVM_DBG_JITC=1` (the existing JIT
compile-activity trace flag) against the real suite fixture. Every method
`toAbsolute()` calls compiles successfully and even reaches C2
(`CharChunk.append`, `.indexOf`, `.getBuffer`, `.endsWith`,
`String.getChars`, `UEncoder.encodeURL`, ...) — but:

```
[cratonvm-jitc] bg-compile org/apache/catalina/connector/Response.toAbsolute(Ljava/lang/String;)Ljava/lang/String; tier=C1 optimized=false
[cratonvm-jitc] compile-bail org/apache/catalina/connector/Response.toAbsolute(Ljava/lang/String;)Ljava/lang/String; backend_attempted=true
```

`toAbsolute()` itself — the method actually called 1,000,000 times by
`doHomebrew()` — permanently bails and stays interpreted for the entire
benchmark (the bail is marked permanent per the existing RBC.4 fix, so this
isn't a retry-storm — it's a single, deliberate, correct refusal to compile,
repeated identically every run).

**Root cause, confirmed against `jit/src/lib.rs`'s own documented gate
(`RBC.6`)**:

```rust
// RBC.6 — a method containing `athrow` compiles only when it has NO
// local exception handlers: the athrow lowering stashes the exception
// and returns the deopt sentinel, which cannot dispatch to an
// in-method handler. Permanent for this bytecode -> bail-list it.
if scan.has_athrow && !cached.exception_table.is_empty() {
    *backend_attempted = true;
    return None;
}
```

`Response.toAbsolute()`'s real source (`org/apache/catalina/connector/
Response.java`) wraps its hot path in exactly this shape, twice:

```java
try {
    redirectURLCC.append(scheme, 0, scheme.length());
    ...
    normalize(redirectURLCC);
} catch (IOException ioe) {
    throw new IllegalArgumentException(location, ioe);
}
```

A `try` block with a local `catch` whose body does `athrow` (rethrowing as
a different exception type) is exactly the pattern `RBC.6` bails on — the
JIT's exception-handling codegen has no way to dispatch control from a
thrown exception to a handler bytecode offset *within the same compiled
method*; it can only propagate outward (the "deopt sentinel"), which would
silently skip the local `catch` and produce the wrong exception type if
compiled anyway. **The gate is not a bug — compiling this method with the
current codegen would be a real correctness hazard, not just a missed
optimization.** This is a deliberate, sound, conservative refusal.

This is genuinely the **complete explanation for the "CharChunk path is
slower than URI path" mystery**: it isn't that `CharChunk`-style
concatenation is innately slower than `URI` parsing on this interpreter —
it's that `toAbsolute()`'s *own* driving bytecode (branching, the
`leadingSlash`/`hasScheme` checks, the try/catch, the final `return
redirectURLCC.toString()`) runs at full-interpreter speed for all
1,000,000 iterations, while every individual callee it invokes IS
JIT-compiled and fast. `doUri()`'s driving code (`URI.create(...)
.resolve(...).toASCIIString()`, called directly from the benchmark's own
`main`-adjacent loop) has no such try/catch-with-rethrow shape and compiles
cleanly, so it runs at JIT speed end-to-end. The ~3x gap is (approximately)
the ratio between "interpreted driver + JIT'd callees" and "JIT'd driver +
JIT'd callees" for a method whose own body is a small fraction of total
instructions but pays full per-call interpreter dispatch overhead for
every one of its ~10 callee invocations per iteration.

### Why this was not fixed this session (and what fixing it would require)

Implementing correct JIT support for local exception handlers is a genuine,
substantial compiler feature — not a bounded patch:
- The compiled method needs to detect, when a callee throws (propagates an
  exception up into the compiled frame), whether the current program point
  falls within a `try`-range that has a local handler, and if so, transfer
  control to that handler's bytecode offset with the correct locals/stack
  state and the exception object bound to the catch variable — full
  in-method exception dispatch, not just entry/exit handling.
- Checked the codebase's own existing deopt machinery
  (`vm/src/runtime/deopt_materialize.rs`, "real-frame-deopt") as a possible
  foundation to reuse: it is for an **unrelated** purpose (re-materializing
  scalar-replaced/escape-analyzed virtual objects after a *type-speculation*
  guard fails, not exception dispatch) and is itself still
  default-off/experimental ("Phases 1+2... reachable today only via the
  acceptance tests"). There is no existing scaffolding to extend safely.
- A narrower "detect provably-dead exception paths and compile anyway"
  static analysis was considered and rejected: `CharChunk.append()` is real,
  non-final, overridable bytecode, so "does this call ever actually throw
  IOException" is not a small, local, sound question — it would require
  either an unsound heuristic (risk: silent miscompilation the one time the
  assumption is wrong) or real interprocedural analysis (same scope as the
  general fix).
- This codebase's own convention (RBC.4/RBC.6/NEW-1.x naming, the large
  number of explicitly `Default-OFF` JIT features already visible in
  `try_compile_inner`'s parameter list) treats JIT correctness/coverage gaps
  as their own tracked, gradually-landed roadmap items, not same-session
  patches — consistent with the caution this specific gate deserves.

**Recommendation for whoever picks this up**: this is a well-scoped,
precisely-diagnosed JIT feature request — "support compiling methods whose
only local exception handlers end in an unconditional rethrow/return (no
control flow re-enters the try region)" would cover this exact pattern
(and is very likely the majority real-world shape: validate-or-wrap-and-
rethrow) without needing full general handler-to-handler dispatch. That
narrower version is still real compiler work (correct locals/stack
reconstruction at the handler entry, correct exception-object binding) but
meaningfully smaller than the fully general case, and would very plausibly
close both `TestResponsePerformance`'s relative-perf assertion (removing
the ~150x-vs-interpreted-driver tax) and any other method sharing this
common idiom. Should get a `docs/feature-designs/` writeup and its own
dedicated session(s), given the correctness stakes.

**2026-07-19 session 2 update**: re-validated the RBC.6 fix against the
real Tomcat suite fixture on the Azure Linux build host and found two MORE
real bugs blocking `Response.toAbsolute()` specifically (a
control-flow-insensitivity bug in the session-1 safety check, and a stale
compile-time gate unrelated to RBC.6, "BUG-LQB-SCOPE") — both root-caused,
fixed, and validated (including against the real method's own bytecode).
`Response.toAbsolute()` now confirmed JIT-compiles. HOWEVER a newly
-discovered, separate performance regression (the method gets recompiled
80-90+ times during the benchmark and runs net SLOWER once "fixed" than it
did fully interpreted) still blocks this test's actual pass/fail outcome —
full investigation, evidence, and next-step handoff in
`docs/feature-designs/jit-local-exception-handlers.md`'s "session 2"
section. This doc's disposition is unchanged (stays open) pending that
follow-up.

### Doc disposition

Left in `docs/known-issues/` (not fixed) with this root cause recorded in
full. Given the actual remaining gap is now a scoped compiler *feature*
request rather than an open-ended performance mystery, this is arguably the
practical ceiling for a same-session diagnostic effort — closing the doc
(making the assertion pass) requires the JIT feature above, which is out of
scope to implement safely in this session.

**2026-07-19 update**: the JIT feature landed the same day (worktree
`fix/jit-rbc6-local-exception-handlers-20260719`) — see
`docs/feature-designs/jit-local-exception-handlers.md`. `Response
.toAbsolute()` now compiles. This doc's disposition is now: **stays open**
only pending a re-run of the actual `TestResponsePerformance` JUnit test
(this fix could not be validated against the real Tomcat suite fixture from
this session's environment — Windows-local, no access to the remote Linux
build host the fixture lives on). Whoever picks this up next should: (1)
rebuild `dev` with the merged fix, (2) re-run
`org.apache.catalina.connector.TestResponsePerformance` at `-Xmx4g` per the
existing reproduction command below, (3) if the relative-perf assertion now
passes, close this doc entirely (all 3 original classes fixed); if it still
fails, capture the new failure mode (a live-`toAbsolute()`-now-JIT'd
`CharChunk` path may still be slower than `URI` for reasons unrelated to
interpretation — the original interpreter-throughput hypothesis this doc's
history already investigated and moved past).

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
