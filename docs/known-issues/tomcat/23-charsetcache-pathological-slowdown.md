# `TestCharsetCachePerformance` — the cached paths lose to the uncached one

**Status:** OPEN. Root-caused (2026-07-27) and partially fixed; the headline
defect is understood but its fix is a JIT project, not a charset one.

Confirmed CratonVM-only — passes on HotSpot in the same fixture.

## Symptom

`org.apache.tomcat.util.buf.TestCharsetCachePerformance.testCache` asserts that
both caches beat the deliberately-uncached baseline:

```java
Assert.assertTrue("No cache was faster than full cache", timeFull < timeNone);
Assert.assertTrue("No cache was faster than lazy cache", timeLazy < timeNone);
```

Measured (10 threads x 10,000,000 lookups per arm):

| arm | HotSpot | CratonVM (dev, 2026-07-27) |
|---|---|---|
| `NoCsCache` (`Charset.forName`) | 34.9s | 60.5s |
| `FullCsCache` (`HashMap`) | 0.71s | 89.9s |
| `LazyCsCache` (real `CharsetCache`) | 0.76s | **909s** |

On HotSpot the caches are ~48x *faster* than the baseline. On CratonVM both are
slower, so the first assertion fails; the `LazyCsCache` arm alone takes 15
minutes, which is what pushes the class past even a 1500s per-class timeout.

## Root cause

**The previous version of this document guessed wrong.** It hypothesised "a
linear scan where a hash lookup is expected, lock contention on every lookup, or
a cache that's being invalidated/rebuilt on every call" inside "whatever
CratonVM-side charset-cache implementation backs `CharsetCache`". No such
component exists: `FullCsCache` is a plain `java.util.HashMap` and `CharsetCache`
a plain `java.util.concurrent.ConcurrentHashMap`, both populated once. The real
causes are generic VM ones that happen to fall on the cached paths and not on
`Charset.forName`.

### Primary: `CharsetCache.getCharset` is never JIT-compiled

This is the whole `LazyCsCache` gap and is worth an order of magnitude.

```
[cratonvm] JIT method stats: ... still-interpreted=2 c1=0 c2=0 hot_but_stuck_in_interpreter=1
[cratonvm]   599988 queued=false tier_fail_count=3
             org/apache/tomcat/util/buf/CharsetCache.getCharset(Ljava/lang/String;)Ljava/nio/charset/Charset;
```

Reproduce with `CRATONVM_DBG_JIT_METHOD_STATS=1`; the refusal reason comes from
`CRATONVM_DBG_JITC=1` (`compile-bail ... backend_attempted=false`) and
`CRATONVM_DBG_RBC6=1`:

```
[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local=true for
           org/apache/tomcat/util/buf/CharsetCache.getCharset(...)
```

The chain, all in `jit/src/lib.rs`:

1. `getCharset` has a `try`/`catch (UnsupportedCharsetException)` whose handler
   reads local 2 (`lcCharsetName`).
2. `local_handler_reads_unsafe_local` treats **only parameter slots** as safe to
   reconstruct in an exception handler, so reading local 2 is "unsafe" — even
   though it is definitely assigned at pc 7, before the protected range [29,45).
   That is not a bug in the analysis: the JIT's params-only handler
   reconstruction genuinely cannot restore a non-parameter local, so compiling
   the method as-is would reset `lcCharsetName` to null in the handler.
3. The escape hatch is `precise_exception_frames`, admitted only when
   `precise_exception_frame_sites_supported` says every throwing opcode in the
   protected range exits through a call site that publishes a precise
   (reason-9) exceptional frame. That whitelist is `invokestatic`,
   `monitorenter`, `monitorexit`.
4. The protected range contains `invokevirtual addToCache` (pc 39), which is not
   whitelisted, so `try_compile_inner` returns `None`.
5. Three such refusals trip `MAX_TIER_FAIL_RETRIES` and the method is never
   attempted again. Every one of the 100,000,000 lookups runs interpreted.

Measured cost of that: ~15-18 microseconds per interpreted `getCharset` versus
~1.5-2.4us for the equivalent compiled `ConcurrentHashMap` probe — and 25ns on
HotSpot.

**Why this was not fixed here.** The obvious fix — add `invokevirtual` /
`invokeinterface` to the whitelist — is *not* safe as written. Several invoke
lowerings in `jit/src/x64.rs` deliberately bypass
`emit_post_invoke_exception_check` (the tail-call form JMPs straight out; see the
`value-stack-usize-underflow-nio-worker-panic` comments at x64.rs:25151 and
:26768), and inlined callees never reach the caller's check at all. Widening the
whitelist without first making every one of those lowerings publish the
snapshot reintroduces exactly the silent-wrong-locals class of bug the check was
added for (`AthrowCountBisect.twoThrowsSequential`,
`vm/tests/jit_local_exception_handler_tests.rs`). Doing it properly is a JIT
exception-frame project: audit each invoke lowering, make it publish the
reason-9 frame, then widen the whitelist and re-run that regression suite.

### Secondary: generic hot-path costs (fixed, see below)

These are why `FullCsCache` also lost, and they are all now fixed. Each was
confirmed with per-site counters and TSC cycle accounting, not inference:

| defect | before | after |
|---|---|---|
| `native_chm_get` had no String-key fast path: two heap-allocated `Vec`s per lookup, the key hashed twice, and 7.2 `class_manager` lock acquisitions per `get` | 6920 cyc/call | 1437 cyc/call |
| `fast_unbox_primitive_wrapper` memoized only *positive* answers, so every String key re-took the `class_manager` read lock and cloned the class's `Arc<str>` name | 1,441,144 locked lookups per 200k gets | 1,685 |
| `jit_typecheck_resolve` re-ran `is_subclass_of` (lock + `FxHashSet` allocation + hierarchy DFS) on every non-exact `checkcast` | 600 cyc/call | 273 cyc/call |
| the JIT HashMap node-cache probe `.clone()`d each entry, heap-allocating its `String` key per entry scanned — in the variant that never reads that key | 1132 cyc/call | 518 cyc/call |
| `JIT_TYPECHECK_TARGET_CACHE` was **single-entry**, keyed by class-name pointer, so two type-check sites in one loop evicted each other every iteration | 1st site ~64ns, 2nd ~1750ns | no cliff |
| every `getstatic` took a `class_manager` read lock (a CAS on one shared cache line) via `ensure_class_initialized_shared` | — | ~10% faster at 10 threads |

## Current state

With those six fixes, on the warm single-thread benchmark
(`WarmCmp`, 500k iterations after warmup):

| probe | baseline | fixed |
|---|---|---|
| `Charset.forName` (control) | 1525 ns/op | 1474 ns/op |
| `HashMap` + `toLowerCase` | 1203 ns/op | 1254 ns/op |
| `ConcurrentHashMap` + `toLowerCase` | 4047 ns/op | 2362 ns/op |
| real `CharsetCache` | 18623 ns/op | 15955 ns/op |

The real `CharsetCache` figure stays an order of magnitude off because it is
still interpreted — that is the primary cause above, untouched.

The test therefore **still fails**, and this document stays open. What changed is
that it now names a specific, verified mechanism instead of a wrong guess.

## Reproduction

Fastest signal (seconds, not minutes) — confirm the method is never compiled:

```powershell
$env:CRATONVM_DBG_JIT_METHOD_STATS='1'
<cratonvm.exe> -cp "<probe>;<tomcat cp>" org.apache.tomcat.util.buf.LazyDiag 200000
```

Look for `hot_but_stuck_in_interpreter=1` naming `CharsetCache.getCharset`. Add
`CRATONVM_DBG_JITC=1` for the `compile-bail ... backend_attempted=false` line and
`CRATONVM_DBG_RBC6=1` for the `local_handler_reads_unsafe_local=true` verdict.

Full class (~20 minutes, mostly the `LazyCsCache` arm):

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> `
  -TimeoutSec 1500 -Parallel 1 -RunName charsetcache-repro -Exe <cratonvm.exe>
```

The printed `NoCsCache`/`FullCsCache`/`LazyCsCache` nanosecond timings land in
`.suite\results\<run>\real-jit\org.apache.tomcat.util.buf.TestCharsetCachePerformance.log`
even when the class times out.

## Not this bug

Distinct from the general interpreter/JIT throughput ceiling in
`29-throughput-wall-recurrence-and-unconfirmed.md` and
`04-embedded-server-throughput-wall-OPEN.md`.
Those are roughly-uniform overhead versus HotSpot. This is a specific method
being refused compilation outright, plus a set of now-fixed hot-path defects.
