# After any Mockito `mock()`, every call into a redefined class costs ~40 µs — Spring's AOT `TestCompiler` step looks like a hang

| | |
|---|---|
| **Status** | **FIXED 2026-07-30.** Root-caused, fixed, and measured: 46x -> 1x on a Mockito-free reproducer. See "Resolution" below. |
| **Category** | VM-PERFORMANCE (JVMTI redefine / interpreter caching) |
| **Found** | 2026-07-26, Azure host `20.83.144.174`, dev `7cb040a97`, real JDK 25. |
| **CratonVM** | ~40,950 ns per `StringBuilder.length()` call after `Mockito.mock(StringBuilder.class)` |
| **HotSpot** | 17 ns per call in the same state |

## Re-measured 2026-07-30, after the per-class gate work

`fix/deep-audit-retire-20260730` replaced the process-wide
`any_class_redefined()` quiesce with exact-class / exact-hierarchy checks
(`vm/src/runtime/redefine_state.rs`), refreshed inherited vtable entries on
reinstall, and stopped one `mock()` from disabling JIT compilation for the whole
process. **That did not close this bug.**

Windows 11, release build, JDK 25, Mockito 5.21, same `SbCostProbe`:

| | before mock | after mock | multiplier |
|---|--:|--:|--:|
| HotSpot 25 | 2 ns/call | 50 ns/call | 25× |
| CratonVM | 1,822 ns/call | **245,653 ns/call** | 134× |

The post-mock multiplier improved (451× on the original Azure run → 134× here),
which is consistent with the gate narrowing doing something real. The absolute
number did not: 245 µs per call against HotSpot's 50 ns is **~4,900× slower**,
and 2M calls take 8 minutes. Absolute figures are not comparable across the two
hosts — this host's *pre*-mock cost is also 23× the Azure run's — so read the
within-run multiplier, not the µs.

Correctness is still fine: a real `StringBuilder` returns the right length after
an unrelated mock.

What this rules out: "the caches were keyed too coarsely" is not the whole
story. The remaining cost is the per-call re-resolution and re-quickening
signature in the profile below, which the generation-keyed caches were expected
to fix and evidently do not. The next step is unchanged — find what produces a
fresh `Arc<[u8]>` for a redefined method's code on every invocation — but it is
now known that narrowing the redefine gates does not get there on its own.

## Resolution (2026-07-30)

**Root cause: a redefined class was permanently de-optimized.** Not cache-key
coarseness, and not the advice body.

Five separate gates asked *"has this class ever been redefined?"* and treated
`yes` as *"permanently unsafe"*. Because the redefinition generation only ever
increases, every one of them stayed true for the life of the process:

| site | effect while permanently true |
|---|---|
| `interpreter.rs` JIT admission | the class can never be compiled again |
| `invoke.rs` OSR entry | no OSR into the class, ever |
| `invoke.rs` compiled-entry lookup | compiled code is never *used* even if present |
| `invoke.rs` compile-callee wrapper | never supplies a compiled callee |
| `jit/helpers.rs` MIC/PIC dispatch | **the inline caches are erased on every single dispatch** |

The last one dominates. A monomorphic inline cache that is cleared on every
call is worse than no inline cache at all: each dispatch re-resolves the target
from scratch and then throws the result away.

None of it was protecting anything. `redefineClass` already calls
`jit_cache.write().clear_all()` plus `invalidate_jit_for_class`, so no code
compiled from the old body survives the redefinition, and any later compilation
necessarily reads the current agent-woven bytecode out of the class store.

**Fix.** The four admission gates are gone — they were redundant with the
eviction. The inline-cache flush became epoch-based: a process-wide
`REDEFINE_EPOCH` is bumped once per `redefineClass`, and each `JitMICSlot`
stamps the epoch it was last validated against. A mismatch flushes that slot
once and restamps it; a match means the slot was populated after the most
recent redefinition and is as trustworthy as any other inline cache. Steady
state is one relaxed atomic load and a compare.

The stamp lives in the 4 bytes that were previously padding at offset 4 of
`JitMICSlot`, so the slot's size and every JIT-hot field offset are unchanged
and generated code needed no update.

### Measured

`docs/known-issues/repros/redefine-call-cost`, Windows 11, release build,
JDK 25, redefining with **byte-identical** bytecode so nothing about the class
changes:

| | before redefine | after redefine | multiplier |
|---|--:|--:|--:|
| before the fix | 691 ns/call | 33,773 ns/call | **46x** |
| after the fix | 314 ns/call | 361 ns/call | **1x** |

The counter that proves the mechanism rather than the timing: with
`CRATONVM_DBG_HOTPATH_COUNTS=1`, `retarget_field` (interpreter field-access
dispatches) climbed to **6,000,000** across the post-redefine phase before the
fix and stays at **9,029** after it. The method stays compiled through the
redefinition instead of falling back to the interpreter forever.

### Correction to "It is NOT the JIT kill-switch (checked)" below

That section is **wrong**, and it is worth understanding why, because the
measurement in it was correct — only the conclusion was not.

```
--nojit   BEFORE 190 ns/call   AFTER 40950 ns/call
jit-on    BEFORE  77 ns/call   AFTER 34656 ns/call
```

This was read as "the post-mock numbers agree, so the JIT is not the factor."
The numbers agree because after the mock **both arms are interpreted** — the
JIT-on arm has degraded *to* the `--nojit` arm. Two arms agreeing is only
evidence that a factor is irrelevant if the factor is still varying between
them, and here the mock had silently removed it. The BEFORE column is where the
signal was: 190 vs 77 ns shows the JIT was doing real work right up until the
redefinition, and never again afterwards.

### Reproducer

`docs/known-issues/repros/redefine-call-cost/` — Mockito removed. Mockito was
only ever a way to reach `Instrumentation.redefineClasses`; the cost was the
VM's. `RedefineCostProbe` measures, `RedefineCorrectnessProbe` guards the fix by
redefining with a body whose arithmetic differs and asserting the new body is
observed both interpreted and after re-tiering. That second assertion was
vacuous while the gates blocked compilation, and is load-bearing now.

### Not claimed

This does not order a compilation already in flight against a concurrent
redefinition: a body compiled from generation N could in principle publish
after the `clear_all()` for generation N+1. That race predates this change and
applies to the first redefinition of any class; closing it wants a publish-time
generation check.

## Reproducer (90 seconds, no Spring, no JUnit)

`/data/data/aot20260726/src/SbCostProbe.java` — times a REAL
`StringBuilder.length()` before and after an unrelated
`Mockito.mock(StringBuilder.class)`:

```
=== HOTSPOT
BEFORE  2000000 length() calls: 1 ms      (0 ns/call)
AFTER   2000000 length() calls: 34 ms     (17 ns/call)      MULTIPLIER 22x
=== CRATONVM
BEFORE  2000000 length() calls: 160 ms    (80 ns/call)
AFTER   2000000 length() calls: 72588 ms  (36294 ns/call)   MULTIPLIER 451x
```

CratonVM is ~2000x slower than HotSpot in the post-mock state.

## Why this presents as a hang

Mockito's inline mock maker redefines `StringBuilder` **and** its
package-private superclass `AbstractStringBuilder` in place, weaving
`MockMethodAdvice` into `AbstractStringBuilder.length()` itself (see
[[mockitobean-length-abstractstringbuilder-fixed-20260723]]). That is correct
and intended — CratonVM must run the woven body so stubbing/verification work.

The problem is what each of those calls now costs. Spring's
`AotIntegrationTests` compiles the generated sources with the in-process javac,
and `JavaTokenizer.scanOperator` calls `StringBuilder.length()` **per token**.
At 40 µs a call the compile never finishes in any practical time. Observed as
`endToEndTestsForBeanOverrides` sitting at 99.4% CPU for 40+ minutes with a
watchdog stack pinned at:

```
javac JavaTokenizer.scanOperator -> StringBuilder.length
  -> AbstractStringBuilder.length -> MockMethodAdvice.isMocked
  -> getSingletonMockInterceptor -> DetachedThreadLocal.get
  -> WeakConcurrentMap.get -> WeakConcurrentMap$LatentKey.hashCode
```

The process is **spinning, not blocked** — 60 s of CPU per 60 s of wall clock.

## It is NOT the JIT kill-switch (checked)

`vm/src/runtime/interpreter.rs` computes
`redefine_jit_quiesced = crate::classloading::any_class_redefined()` and ORs it
into the JIT skip condition, so one `mock()` anywhere does permanently disable
JIT compilation process-wide. That is worth revisiting on its own, but it is
**not** the dominant cost here:

```
--nojit   BEFORE 190 ns/call   AFTER 40950 ns/call   (215x)
jit-on    BEFORE  77 ns/call   AFTER 34656 ns/call   (448x)
```

The post-mock numbers agree, and the 215x appears *within the interpreter
alone*. Executing ~30 bytecodes of advice plus a `ConcurrentHashMap` lookup
should cost ~1-2 µs interpreted, not 40 µs.

## Where the time actually goes

`perf record -F 199` on the spinning process (3977 samples), top self-time:

```
 6.74%  _mi_page_malloc_zero
 5.28%  interpreter::execute_instruction
 4.80%  interpreter::execute_frame_from_index
 3.97%  NativeMethodRegistry::slot_for_exact
 3.47%  interpreter::execute_invokevirtual_cached
 3.34%  interpreter::resolve_method_metadata
 3.09%  interpreter::resolve_field_ref_loader_aware
 2.77%  native_builtins::classloader::defining_loader_for
 2.34%  cratonvm_reader::quickened::intern
 2.29%  cratonvm_reader::quickened::QuickenedCode::build
 2.14%  vm_init::SharedVm::load_class_concurrent
 1.76%  drop_in_place<QuickenedCode>
 1.73%  NativeContextImpl::class_name_of_id
 1.63%  interpreter::lookup_loader_initiated
 1.46%  drop_in_place<OrderedPlRwLockReadGuard<ClassManager>>
 1.08%  interpreter::should_force_registered_native_over_bytecode
```

That is a **per-call re-resolution and re-quickening** signature, not the cost
of running the advice:

- `QuickenedCode::build` **and** `drop_in_place<QuickenedCode>` both hot means
  the quickened bytecode stream is rebuilt and thrown away on every single
  invocation. `quickened::intern` (`reader/src/quickened.rs`) keys on
  `(code.as_ptr(), code.len())` and holds a strong `Arc<[u8]>` per entry, so a
  hit is impossible only if a **fresh `Arc<[u8]>` is produced per call**.
  `SHARD_CAP` is 8192 x 16 shards, so this is not cache-capacity thrash — that
  was checked.
- `resolve_method_metadata` hot means `SharedVm::resolution_cache` is missing
  every time too, even though it is only supposed to be invalidated once, at
  redefine time.
- `load_class_concurrent`, `defining_loader_for`, `lookup_loader_initiated`,
  `class_name_of_id` and a `ClassManager` read guard per call mean the whole
  slow `execute()` path (string class names, registry `find`, immunity-list
  string compares) runs for every invocation instead of the cached
  invoke path.

## Next step

Find what produces a fresh `Arc<[u8]>` for a redefined method's code on each
invocation, and why a redefined-class target is never cached by
`execute_invokevirtual_cached` / `resolution_cache`. The likely shape of the
fix is to key the caches on `(class_id, redefine_generation)` and repopulate
after a redefine, rather than permanently falling back to the uncached path
whenever `any_class_redefined()` is true. Use `SbCostProbe` to measure — it
turns a 40-minute AOT run into a 90-second check.

Two things NOT to try:

- Putting `length` back on `redefine_immune_string_builder_native`'s
  blanket-immune list. That makes real `length()` fast again but re-breaks
  `verify(mock).length()` / `when(mock.length())` — it is exactly what the
  2026-07-23 session removed, deliberately.
- The `HttpURLConnection` "real carrier = field 0 non-null" receiver trick. It
  does not generalise to StringBuilder; CratonVM allocates a mocked
  `StringBuilder` through the same synthetic path as a real one, buffer and all
  (see [[aot-followup10-blocker2-cglib-stringbuilder-closed]]).

## Correctness is fine

`RealAfterMockLengthProbe` passes on current dev — a real `StringBuilder` after
an unrelated mock returns the right length, and
`TypeUtils.parseType("java.lang.reflect.Method[]")` returns
`[Ljava/lang/reflect/Method;`. This is purely a throughput defect.
