# `TestCharsetCachePerformance` — the cached paths lose to the uncached one

**Status:** OPEN (last measured 2026-07-31). Both assertions still fail, but
the reason has changed and is now specific. **Read the 2026-07-31 section at
the end first — it supersedes every diagnosis above it.**

The thread-scaling wall the previous status described is fixed: at the real
test parameters the three arms are 3.7x / 2.6x / 5.9x faster, and the
`LazyCsCache` arm alone went 459.5s → 76s. What is left is not a charset-cache
problem, not a dispatch problem and not a scaling problem — it is the flat
~750 ns CratonVM pays per *native call*, where HotSpot pays ~4 ns. The test
compares one arm that makes one native call against two arms that make two, so
it cannot pass until that floor comes down. The arithmetic is in *What the
test actually needs now*.

Everything between here and that section is kept as the record of how the
earlier layers were peeled off. Several of its conclusions were corrected
later; the corrections say so where they are.

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
`../../internal/fixed-suite-bugs/tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md` and
`../../internal/fixed-suite-bugs/tomcat/04-embedded-server-throughput-wall-CLOSED.md`
(retired 2026-07-27; its synchronized-method admission residual was fixed and
retired as `31-synchronized-code-never-jit-compiled-FIXED.md`).
Those are roughly-uniform overhead versus HotSpot. This is a specific method
being refused compilation outright, plus a set of now-fixed hot-path defects.

## Update 2026-07-27 — RBC.6 gate removed; first assertion now passes

The "why this was not fixed here" reservation above has been resolved. The
`0xb6 | 0xb7 | 0xb9` codegen arm was audited: all four of its exits are safe
(two emit no call at all; the inline path is already unreachable because
`try_compile_inner` clears `inline_sites` under `precise_exception_frames`; and
the direct-call and MIC/`jit_invoke_dispatch` paths both end in
`emit_post_invoke_exception_check`). The only lowering that could not honour
the contract was the **sibling tail-call**, which tears the frame down before
the callee runs — a latent hole for `invokestatic`, which the whitelist had
always admitted, not something the new opcodes introduced. It is now suppressed
inside protected ranges (`x64::Compiler::pc_is_protected`, fed by a one-shot
thread-local carrying the exception table's `[start_pc, end_pc)` ranges).

`precise_exception_frame_sites_supported` now admits 0xb6/0xb7/0xb8/0xb9 plus
the monitor ops. `invokedynamic` (0xba) is still excluded — it lowers to an
unconditional deopt trap, not a frame-publishing call site.

Result on the real class:

| arm | before | after |
|---|---|---|
| `NoCsCache` (control) | 60.5s | 60.8s |
| `FullCsCache` | 89.9s | **36.7s** |
| `LazyCsCache` | 909s | 835s |

`assertTrue(timeFull < timeNone)` — **PASSES** (full/none 1.49 → 0.60).
`assertTrue(timeLazy < timeNone)` — still fails (13.7).

`CharsetCache.getCharset` is confirmed compiled now
(`hot_but_stuck_in_interpreter` 1 → 0, `c1=1`).

### The remaining `LazyCsCache` gap is NOT admission

With `getCharset` compiled, the same binary gives wildly different per-op costs
depending only on the *driver* shape:

* `LazyDiag` (single thread, direct `CharsetCache` local, loop in `main`):
  **1961 ns/op**
* `WarmCmp` (single thread, call through a one-method interface):
  **12152 ns/op**

Both compile `getCharset`. Call-site polymorphism was ruled out — running the
lazy probe *first*, while its call site is still monomorphic, measures the same
~12000 ns/op. So the residual is a third factor, not yet identified, and it is
what the surviving assertion is measuring. That is the next thing to chase for
this doc; the admission gate is no longer in the way.

Validation for the gate change: `jit_local_exception_handler_tests` 8/8 (the
suite guarding this exact property, including the
`AthrowCountBisect.twoThrowsSequential` silent-wrong-checksum regression the
params-only rule was added for), plus a `HandlerLocals` conformance probe
(handler reads a local assigned before the try / inside the try / across
sequential and nested try blocks / behind a `synchronized` block / reassigned
after the call, with the throwing call reached through each invoke opcode
including one in tail position) — byte-identical on CratonVM and HotSpot.

## Update 2026-07-27 (2) — the third factor: try/catch excludes a method from C2

The residual flagged above ("NOT admission … a third factor, not yet
identified") is now identified: **a method with an exception table never enters
the optimizing IR pipeline at all**, so it is permanently limited to
single-pass-backend code quality — worth ~7x here.

`jit/src/lib.rs:7831` gates the whole IR/C2 path on
`cached.exception_table.is_empty()`. The rationale is in the STUB-S8 comment
directly above it: the IR builder has no exception-table-aware codegen — a
handler entry is not a registered merge target, so the builder would walk
handler bytecode with stale `ctrl`/`locals`/`stack` from wherever the linear PC
walk last was, producing orphaned nodes referencing `NO_NODE` that the
scheduler/lowerer still visit (panicking in `ir_lower::slot_of` on a `u32::MAX`
index).

Measured with `LazyIsolate`, which changes exactly one thing per variant —
300k iterations, single thread, same binary, same run:

| variant | CratonVM | HotSpot |
|---|---|---|
| d4 clone of `getCharset`, handler reads a non-param local, `try` | 8252 ns/op | 74 |
| d5 **identical, `try`/`catch` removed** | **1094 ns/op** | 85 |
| d8 same but handler reads ONLY parameters, `try` | 7091 ns/op | 17 |
| d9 **identical, `try`/`catch` removed** | **1149 ns/op** | 61 |
| d7 plain `ConcurrentHashMap` control | 1339 ns/op | 72 |

d8/d9 are the important pair: that handler reads only parameters, so
`local_handler_reads_unsafe_local` is false and `precise_exception_frames` is
never engaged — and it is still ~6x slower than its no-`try` twin. So this is
**not** the RBC.6 escape hatch being expensive; it is the flat
`exception_table.is_empty()` requirement on C2. The catch block is never
entered in any variant; the cost is entirely static.

That closes out the "why is `LazyCsCache` still slow" question for this doc:
`CharsetCache.getCharset` has a `try`/`catch (UnsupportedCharsetException)`,
so even now that it compiles it can only ever be C1-quality.

### What a fix requires

Exception-table support in the IR builder: register each handler entry as a
merge target with a correctly-typed `ctrl`/`locals`/`stack` state, so the
handler's bytecode is built as a real CFG block rather than walked over. That
is a self-contained but non-trivial IR project, and it would lift a ceiling
that applies to **every** `try`/`catch` method in every workload — not just
this test. It is almost certainly worth more than anything else named in this
document.

Reproduce the measurement with the `LazyIsolate` probe shape above; the d5/d9
"delete the try/catch, change nothing else" control is what makes it airtight.

## Correction 2026-07-28 — the C2 exclusion is real, but it is NOT what d8/d9 measured

The section above identified the optimizing tier's blanket refusal of
exception-table methods (`cached.exception_table.is_empty()` in
`try_compile_inner`) and attributed the measured d8-vs-d9 gap to it. **The
exclusion was real and is now fixed; the attribution was wrong.**

What the fix changed (jit/src/ir.rs, jit/src/lib.rs, reader/src/verified_code.rs):
the IR builder now skips handler bodies — they are unreachable in a compiled
frame, because a JIT frame never enters its own handler — so an exception table
no longer disqualifies a method from the optimizing tier. Verified: a
`try`/`catch` method reaches the IR backend (`try_catch_param_only_handler_uses_ir`),
and the same method emits **401 bytes via C2 vs 482 via single-pass** with the
new `CRATONVM_JIT_NO_EXC_TABLE_C2` opt-out flipped.

Why it does not explain the d8/d9 gap. Measured A/B on ONE binary with that
flag, which is the only rigorous control:

| | fix ON | fix OFF | compiled size |
|---|---|---|---|
| d8 (`try`) | ~4200 ns/op | ~4200 ns/op | `len=1954` both |
| d9 (no `try`) | ~610 ns/op | ~610 ns/op | `len=1949` |

d8's code is byte-for-byte the same size with the fix on and off: it never
reached the optimizing tier either way (`ir_compatible` holds it off — it is
call-heavy — not the exception table). And the two bodies compile to within five
bytes of each other while running 7x apart, which rules out code quality as the
cause on its own.

The actual cause of the 7x is caller-side and is written up separately:
[a callee that declares an exception table is barred from the inline-cache fast
path](../../internal/jit-bans/exception-table-callee-barred-from-inline-cache-20260728.md)
(itself FIXED 2026-07-30).
Every call to such a method pays the full dispatch helper instead of the inline
cascade, because the helper is the only place that can route a pending exception
through the callee's own table.

**Methodological note on how the wrong attribution happened.** The 2026-07-27
numbers were taken by comparing two separately built binaries. The comparison
binary turned out to be a feature branch **260 commits behind dev**, so the
measurement confounded the try/catch variable with 260 commits of unrelated JIT
work — visible in hindsight because d9, which has no `try`/`catch` at all and
therefore cannot be affected by any exception-table gate, also moved by 12x
between the two binaries. Always A/B a single binary against itself behind a
flag; if a control that *cannot* move does move, the comparison is invalid.

**Consequence for this document:** `timeLazy` is still expected to fail, and the
reason is now the inline-cache ban, not the tier. Doc 23 stays OPEN.

## Update 2026-07-30 — the `getfield`/`putfield` widening was UNSOUND and is reverted

`0xb4`/`0xb5` were added to `precise_exception_frame_sites_supported`'s exempt
list on 2026-07-28 in `5bf306bb0` ("jit: compile synchronized Tomcat paths
safely"). **That widening produced silent wrong values and has been removed.**

This is the case this document warned about: the whitelist was widened without
auditing that *every* lowering the opcode can select publishes a reason-9 frame.
`5bf306bb0` did add one — `emit_precise_null_check_field_store` — but it has a
single call site, on the **inlined-callee** `putfield` path. The top-level field
arms never got it.

### Measured, both directions

`probes/Rbc6FieldProbe.java` builds the exact shape RBC.6 protects: a protected
range containing a field access on a null receiver, a non-parameter local
written inside the `try`, and a handler that reads it.

| probe | HotSpot 25 | CratonVM `--nojit` | CratonVM JIT, `0xb4/0xb5` exempt |
|---|---|---|---|
| `getfieldInt(null,5)` | 38 | 38 | **0** |
| `putfield(null,5)` | 66 | 66 | **-1** |
| `twoLocals(null,5)` | 105205 | 105205 | **0** |
| `getfieldRef(null,5)` | 60 | 60 | 60 |
| `getfieldLong(null,5)` | 5000015 | 5000015 | 5000015 |
| whole-run `acc` | 3505302427599075008 | same | **3505299872972359454** |

`0` is the handler reading a zeroed non-parameter local — params-only
reconstruction, exactly the silent-wrong-locals hazard. With `0xb4`/`0xb5`
removed from the list, every figure matches HotSpot and `--nojit` including the
checksum, and `cargo test -p cratonvm-jit` is fully green (1056 lib tests, all
integration binaries).

Note the reference-typed and category-2 loads were *already* correct — they take
the helper exit, which does publish. Only the inline fast path was wrong, which
is why reading the arm for "does it publish?" is not enough: **the question is
which lowering each opcode can select, not whether some lowering is safe.**

### The regression test that already said so

`tests::protected_field_access_keeps_unsafe_handler_interpreted` (added
2026-07-27, `83e078aa5`) asserts exactly this refusal. `5bf306bb0` landed one day
later and broke it, without mentioning it. It then went unseen for two days
because `cargo test -p cratonvm-jit` could not compile (fixed `4d8a39a39`) and
then SIGSEGV'd before reaching it (fixed `fcc723007`). The test was right the
whole time; it is now passing again, unmodified.

### What this costs

Methods with a field access inside a `try` whose handler reads a non-parameter
local go back to being interpreted. That is a real throughput loss and it takes
back part of what `5bf306bb0` was reaching for. It is not negotiable against
silently wrong results, and the note in the exempt list now records the
measurement so the next person does not re-widen it by inspection.

The way to earn the widening back is the one this document has always specified:
publish a precise reason-9 frame at the **top-level** `getfield`/`putfield`
arms — including the inline fast path — and only then re-admit the opcodes,
with `Rbc6FieldProbe` as the acceptance test.

### Separate defect found by the same probe — `putfield` on null does not throw

`probes/NullPutfieldProbe.java` puts the store in a method with **no exception
table at all**, so RBC.6 never applies:

| | HotSpot | CratonVM JIT |
|---|---|---|
| `putfield` int / ref / long on a null receiver | NPE | **NO-THROW** |
| `getfield` int on a null receiver | NPE | NPE |

A compiled `putfield` to a null receiver silently drops the store and continues.
This is independent of the gate above and was not fixed by the revert.

**FIXED 2026-07-30.** The top-level `0xb5` arm now calls
`emit_precise_null_check_field_store` on its receiver, exactly as its
inlined-callee sibling already did — the single place that call was missed when
it landed in `5bf306bb0`. The scalar-replaced branch is deliberately excluded:
its "objectref" is a dummy with no receiver behind it. Root cause on the helper
side, for the record: `jit_putfield_*` guards with
`if !plausible_heap_pointer(obj_ptr) { return; }`, which avoids dereferencing
garbage but returns **without raising**, so nothing ever threw.

`probes/NullPutfieldProbe.java` now reports NPE on all four rows, matching
HotSpot. Cost measured interleaved against a pre-fix binary: none on
`BinTreesClassic` d=18 (base median 2120 ms vs fix 2113 ms, checksum 68332206
both, host noise ±30% swamping the difference), and ~5-9% only on
`probes/PutfieldPerfProbe.java`, a deliberately pathological loop that does
nothing but field stores. An implicit null check (fault + signal translation,
as HotSpot does) was therefore not pursued — there is no measured cost to
recover.

**The same defect in the other tier is now FIXED too** (2026-07-30). The IR/C2
lowering in `jit/src/ir_lower.rs` (`Op::Store`) described its own behaviour as
"null receiver → no-op" and jumped over the store with a `JE +27`. It now emits
`emit_deopt_if_zero(bci, DeoptReason::NullCheck)` — the identical guard the
neighbouring `ArrayLoad`/`ArrayStore` arms already used for a null array — so
control leaves for the shared deopt stub and the interpreter re-executes the
`putfield` and throws. No new machinery was needed: contrary to the worry that
the IR lowerer cannot raise mid-graph, it has raised from array guards all
along.

**Reaching it takes TWO gates open, not one.** Besides the optimizing tier
being off by default (see
`jit-optimizing-tier-disabled-by-moving-young-default.md`), the IR *builder*
bails out of `putfield` whenever `compact_ref_fields_enabled()` — which
defaults to **true** (`Err(_) => true` in `types/src/field_layout.rs`). So the
repro needs both:

```bash
CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_COMPACT_REF_FIELDS=0 \
  <cratonvm> --java-home <jdk25> -cp probeout NullPutfieldProbe 400000
```

Under that configuration the pre-fix binary reports `putfield-int =NO-THROW`
and everything else `NPE` — exactly right, because the IR builder only lowers
**int-category** fields (`I Z B C S`) to `Op::Store`; reference and long stores
never reach this tier and took the single-pass path fixed in `cd451faccc`. A
one-row failure is the signature of this bug, not a partial repro.

Cost: none. The guard replaces `TEST+JE` with `TEST+Jcc`-to-stub, the same
fast-path shape. Interleaved against a pre-fix binary in the IR-live
configuration: `PutfieldPerfProbe` 20M is indistinguishable (ints 122.2 ms both,
wide 126.7 ms both), and `BinTreesClassic` d=18 shows base median 2608 ms vs fix
2524 ms with checksum 68332206 on every run.

*(Superseded 2026-07-30: that inline-cache ban has since been fixed, and
`timeLazy` still fails. See the final section.)*

## Update 2026-07-30 (2) — `timeFull` passes on merit; the residual is a thread-scaling wall

Measured on the Azure Linux build host (16 cores, Temurin 25.0.3, load < 2),
`origin/dev` @ `e255f60fb1` versus this branch merged on top of it,
**interleaved on one host, three rounds each**, because the arms drift seconds
run to run. Medians of the real class:

| arm | dev | this branch | change |
|---|---|---|---|
| `NoCsCache` (control) | 35.8s | 39.3s | +10%, unexplained — read the caveat |
| `FullCsCache` | 49.4s | 37.1s | **-25%** |
| `LazyCsCache` | 243.3s | 157.8s | **-35%** |

`assertTrue(timeLazy < timeNone)` still fails, by 4.0x rather than 6.8x.
**Doc 23 stays OPEN on it.**

`assertTrue(timeFull < timeNone)` is now **flaky rather than fixed**, and that
distinction matters. Its ratio moved from 1.36-1.43 on dev (fails every time) to
a median 0.95 here — but across seven runs of this branch it came out
0.89 / 0.94 / 0.95 / 0.95 / 0.98 / 1.09 / 1.14, i.e. **5 pass, 2 fail**. The
`FullCsCache` arm itself is steady (34.5-37.2s across those seven). All the
movement is in the control: `NoCsCache` alone spans **31.2s-39.8s**. Anyone
re-running this class should expect the first assertion to flip.

**Where the control arm's spread comes from — and a warning.** Within any one
sweep the control looks tight and *systematic*: three interleaved rounds gave
35.6-36.0s on dev and 39.0-39.4s here, which reads as a clean 10% regression.
Over more runs it is not: the arm measured alone (`NoArmOnly`, the arm's code
copied verbatim, four runs per binary) gives 31.9s vs 31.7s with runs spanning
27s-37s on both, and `ForNameProbe2` shows `Charset.forName` itself unchanged
(2641 -> 2635 ns/op at one thread, 9281 -> 9232 at ten). `Charset.forName` is a
native here (`native_charset_for_name`) whose profile is ~14%
`RawMutex::lock_slow` on both binaries, and the two profiles are otherwise
indistinguishable. Consecutive runs on this host correlate strongly, so a tight
triple is not evidence — this arm needs 8+ runs before any claim about it, in
either direction.

Judge the fix on the two arms it targets, where the effect is far outside that
noise: 49.4s -> 37.1s and 243.3s -> 157.8s.

### What landed

1. `native_chm_get` grew a per-thread String-node memo keyed by `(map, exact key
   String object)` and validated against a striped generation counter that every
   `ChmMonitorGuard` advances. A validated hit skips the String hash, the
   segment-array walk and the segment identity-hash mint.
2. That fast path's chain walk became optimistic: it reads the stripe's resize
   seqlock instead of taking the stripe read lock, and redoes the walk under the
   lock only when a resize overlapped it.
3. The `String.to{Lower,Upper}Case` per-receiver memo became Locale-aware
   (`(source, locale, upper)` rather than `(source, upper)`) and is checked
   *before* the synthetic-Locale language lookup, so a repeated ASCII fold no
   longer takes that contended path. With the key fixed, the four real-JDK
   registrations that had been deliberately left uncached could adopt it.
4. `StringLatin1.toLowerCase(String,byte[],Locale)` became a named callback, and
   the interpreter's cached-invoke paths grew fast arms for it, for
   `String.toLowerCase(Locale)` and for `Map.get(Object)`. Those cache entries
   already prove receiver class and callback identity, yet the generic arm still
   re-resolved the CP descriptor and allocated an argument `Vec` per call.
5. The `HashMap` node memo is keyed on the key object instead of re-comparing
   String contents on every probe.

Isolated (`ForNameProbe2`): `toLowerCase(Locale.ENGLISH)` on a repeated receiver
goes 2347 -> 1977 ns/op at one thread and 12536 -> 9819 ns/op at ten. A
never-repeating receiver, where the memo cannot hit and can only cost, is
unchanged at one thread and 8% faster at ten. `Charset.forName` itself is
unchanged (2641 -> 2635 ns/op at one thread, 9281 -> 9232 at ten).

### Three correctness defects in that memo, and how they were caught

The memo as first written returned **the segment object itself** from
`ConcurrentHashMap.get`. `native_chm_get_string_chain` published the node to the
memo *before* the `computeIfAbsent` reservation-marker filter, and a memo hit
answers later lookups without ever re-running that filter — so once a reader
observed a key mid-`computeIfAbsent`, every later lookup of that key on that
thread returned the internal marker. `ChmMemoStressProbe` (readers pinned to one
key object, writers replacing and removing it, a grower forcing segment
relinking, and a slow `computeIfAbsent` holding a marker) prints
`FAIL reservation marker leaked: class cratonvm.synthetic.AnonymousObject$3`
within seconds on that build, and passes on dev and on the fixed branch. The fix
is structural: the chain walk hands back a memoizable node *only* for a value
that survived the marker filter.

Two more, from reviewing the same code:

* `ChmSegmentResizeGuard` incremented the resize epoch **before** taking the
  stripe write lock, so two resizers sharing a stripe could interleave their
  increments and leave the epoch EVEN while one was still relinking — exactly
  what an optimistic reader reads as "no resize in progress". Lock first, then
  mark.
* The generation counter was a parity word protected by a striped
  `parking_lot::Mutex` held across `ChmMonitorGuard`'s `monitor_enter`, i.e.
  across a GC-safepoint park. A thread blocked on that mutex is not at a
  safepoint, so it stalls every collection queued behind it, and a nested guard
  on a same-stripe segment closes an AB-BA cycle outright. Replaced with a
  lock-free epoch plus an in-flight-mutator count: a reader may memoize only if
  it saw zero in-flight mutators on both sides of its walk with an unchanged
  epoch, which catches every overlapping mutator without serializing writers at
  all.

**Do not "harden" a memo hit with a `node.key == key` identity test.** The entry
is published after a *content* comparison, and a CHM's stored key is almost never
the same object as the lookup key — `CharsetCache` stores the name it built at
construction and looks up a freshly lower-cased one. That test misses on every
hit and silently disables the memo: it cost the `LazyCsCache` arm 156s -> 201s
before the A/B above caught it.

### The re-widened RBC.6 whitelist was reverted — it now buys nothing

A first draft of this work re-added `0xb6`/`0xb9` to
`precise_exception_frame_sites_supported`, restoring the 2026-07-27 widening
described earlier in this document. That widening had been **removed again on
dev the day before**, by `a523715a84`, together with the
`protected_precise_handler_call` suppression in `x64.rs` — because it let
Spring's `SimpleApplicationEventMulticaster.invokeListener` compile and then
read its own pre-`try` `errorHandler` local back as null inside the handler
(`springboot-rerun-20260728-small-residuals-cluster`, Case 3).

Re-widening was A/B'd on this branch against itself, one line apart, two rounds:

| arm | whitelist widened | whitelist as dev has it |
|---|---|---|
| `FullCsCache` | 36.3s / 38.7s | 36.0s / 37.5s |
| `LazyCsCache` | 154.5s / 159.5s | 159.0s / 157.3s |

**Indistinguishable.** The 2026-07-27 reasoning — that this whitelist is what
lets `CharsetCache.getCharset` compile at all — no longer holds: `try`/`catch`
methods now reach the optimizing tier by skipping handler bodies (`c05f85967a`),
and the inline-cache ban on exception-table callees is itself fixed
(`675799f47a`). So the widening would carry a known Spring correctness risk for
zero measured throughput, and this branch keeps dev's narrower list. The unit
test now pins the narrow contract and records what a future re-widening would
have to re-verify first.

### The real residual: CratonVM does not scale on this shape at all

This is a **ten-thread** benchmark, which every earlier analysis in this document
treated as incidental. `ScaleProbe` runs the same arm shapes at 1, 2, 4 and 10
threads and reports ns/op *per thread*:

| probe | dev 1t | dev 10t | branch 1t | branch 10t | HotSpot 1t | HotSpot 10t |
|---|---|---|---|---|---|---|
| `HashMap.get(name.toLowerCase(L))` | 987 | 4891 | 699 | 3563 | 20.2 | 33.1 |
| `toLowerCase(Locale)` alone | 768 | 4230 | 498 | 3471 | 11.3 | 18.8 |
| `HashMap.get` alone | 308 | 3810 | 182 | 3943 | 6.6 | 12.7 |
| `ConcurrentHashMap.get` alone | 1011 | 13081 | 480 | 3452 | 6.8 | 13.1 |

At ten threads all four converge on ~3500-3900 ns/op **regardless of how much
work the arm does** — a 3.8x spread at one thread collapses to nothing, and
aggregate throughput is nearly flat from 1 to 10 threads. HotSpot's spread
survives. The workload is serialized on something shared, and it is not the map
and not the case conversion.

`perf record -F 499` on that ten-thread run, flat:

```
13.45%  parking_lot::raw_rwlock::RawRwLock::lock_shared_slow
 8.97%  parking_lot::raw_mutex::RawMutex::lock_slow
 7.20%  [kernel]                                            (futex)
 6.75%  cratonvm_vm::jit::helpers::virtual_dispatch_target_for_receiver
 5.73%  cratonvm_vm::runtime::interpreter::try_jit_compile_callee
 4.68%  cratonvm_vm::jit::helpers::jit_getstatic
 4.46%  cratonvm_vm::jit::helpers::jit_invoke_virtual_mic
 2.98%  cratonvm_native_api::registry::NativeMethodRegistry::slot_for_exact
```

About a third of all CPU is lock acquisition, and the *parking* variants at
that. `CRATONVM_DBG_MIC_PROF=1` names the path: **`hit_noentry` is 33% of all
MIC calls** (3,762,994 of 11,461,404). That is the arm where the monomorphic
inline cache matches the receiver class but holds no compiled entry, so the call

1. calls `virtual_dispatch_target_for_receiver`, which takes
   `vm.classes.class_manager.read()` just to turn the receiver's ClassId into a
   name;
2. then, in `jit_invoke_dispatch`, takes a **second** `class_manager.read()` for
   `get_loaded_class_id(&target.class_name) == receiver_cid`; and
3. re-runs `try_jit_compile_callee` for a callee that will never yield an entry.

A **native** callee can never have a compiled entry, and both hot operations here
— `String.toLowerCase(Locale)` and `Map.get` — are natives in CratonVM. So every
one of the 100,000,000 lookups takes two acquisitions of one process-wide
`RwLock` plus a failed compile probe. Reader-reader `parking_lot` contention on a
single cache line at ten threads is exactly the flat ~3700 ns/op ceiling above.

**The repair, and what it actually moved.** Done — see the final section.
It required a class-definition epoch on `ClassManager` first, because
`globally_named` is `get_loaded_class_id(name) == receiver_cid` and that answer
flips the moment a second loader defines the same name, which none of the three
existing flush signals can see.

The single-thread gap that remains (675 vs 20.2 ns/op on the full-cache arm) is
the separate, already-known native-call dispatch overhead, and is not addressed
here either.

### Reproduction

```bash
# the arms, isolated, with a thread-count sweep — this is the useful one
<cratonvm> --java-home <jdk25> -cp <probes> ScaleProbe 300000
# the control arm alone; it is noisy, expect 27-40s, run it 4+ times per binary
<cratonvm> --java-home <jdk25> -cp <probes> NoArmOnly
# the memo's correctness net (fails in seconds on a memo that caches markers)
<cratonvm> --java-home <jdk25> -cp <probes> ChmMemoStressProbe 20
# where the ten-thread time goes
perf record -F 499 -- <cratonvm> ... ScaleProbe 300000
perf report --stdio --sort symbol
CRATONVM_DBG_MIC_PROF=1 <cratonvm> ... ScaleProbe 100000    # hit_noentry share
```

Regression check for the change set: the 62 `org.apache.tomcat.util.{buf,
collections,http}` / `org.apache.catalina.util` classes run identically on dev
and on this branch — 59 PASS, the same two `*LargeHeap` failures (they want more
than the 2g used here) and the same `TestMethodPerformance` timeout on both.

## Update 2026-07-30 (3) — the dispatch-target memo landed; the wall moved to a different lock

The memo named in the previous section is implemented. `ClassManager` now
carries a **class-definition epoch** (`class_definition_epoch`), bumped by the
three mutations of `loaded_classes` — insert, remove, and the bulk retain in
`unload_user_classes_inner` — which are the only state
`get_loaded_class_id` consults. `vm/src/jit/helpers.rs` memoizes
`(call site, receiver ClassId) -> (dispatch class name, cacheable_receiver,
globally_named)` per thread, revalidated against that epoch, and the three hot
resolution sites (`jit_invoke_dispatch`, and both the no-entry and full-miss
arms of `jit_invoke_virtual_mic`) now read it instead of taking
`class_manager.read()` twice per call.

### It removed exactly what it was aimed at

`perf record -F 499`, ten-thread `ScaleProbe`, same host, same run shape:

| symbol | dev | with the memo |
|---|---|---|
| `RawRwLock::lock_shared_slow` | 12.50% | **3.18%** |
| `virtual_dispatch_target_for_receiver` | 7.72% | **gone** |
| `OrderedPlRwLock<T>::read` | 2.07% | below 1.5% |

That is ~19 points of CPU taken off the `class_manager` read lock.

### But the wall-clock barely moved, because the next lock is bigger

| symbol | dev | with the memo |
|---|---|---|
| `RawMutex::lock_slow` | 7.71% | **12.12%** |
| kernel (futex) | ~6% | ~12% |

Total lock time is roughly conserved: the workload is still serialized, now on a
plain `Mutex` rather than the `class_manager` `RwLock`. Ten-thread `ScaleProbe`
moved from 3803/3011/3664/3540 ns/op to 3001/3412/3709/3253 — inside the noise
for three of the four arms. Single-threaded cost is unchanged (`Solo`, eight
runs per binary: median 826 ns/op both, means 837 vs 841), so the memo's hash
probe costs what the uncontended lock it replaced cost.

**The Mutex is `types/src/jit_activation.rs`.** It is one process-wide
`Mutex<Registry>` holding two `FxHashMap`s, and `enter`/`exit` lock it on
**every** compiled-frame activation — twice per JIT boundary crossing, from
every mutator at once. It exists to keep a defining loader alive while one of
its compiled frames is on the stack. `owners` is written only when code is
published and read on every enter; `active` is a refcount per ClassId that only
`active_class_ids()` (the unload path) ever reads in aggregate. Both halves are
therefore fixable — a read-mostly structure for `owners`, per-thread counters
walked at aggregation time for `active` — but that is class-unload safety
machinery, a separate change from this one.

Two smaller `class_manager` acquisitions also survive on the same path, both
inside `try_jit_compile_callee`, which the no-entry arm calls every time:
`named_method_is_synchronized` takes a read lock, and the JIT-cache probe takes
another for `get_loaded_class_id(class_name)` — an answer the new memo has
already computed and could hand it. Worth doing, but at 3.18% it is no longer
where the time is.

### Safety argument for the memo

Two of the three memoized fields need no invalidation at all: **`ClassId`s are
never reused.** `ClassStore` tombstones an unloaded slot (`classes: Vec<Option<
Class>>`, with `unloaded_slots_are_tombstoned_and_never_reused` asserting it),
so a live receiver's `ClassId` always denotes the same class, and its name and
`ACC_INTERFACE` are fixed at definition. Only `globally_named` is mutable, and
that is what the epoch tracks.

A stale `globally_named` is additionally not exploitable today, because its one
by-name consumer, `try_jit_compile_callee`, re-resolves the name under its own
lock and `get_loaded_class_id` returns `None` on a multi-loader collision rather
than guessing. The epoch is what makes the memo *locally* provable instead of
resting on that distant invariant — and, as a side effect, it closes a gap
`DISPATCH_CACHE` and `VIRTUAL_DISPATCH_CACHE` already had: they cache a compiled
entry chosen on the strength of a `globally_named` test that nothing was
re-running, and they now flush on the epoch too.

`probes/LoaderNameCollisionDispatchProbe.java` is the regression net: two
sibling loaders each define their own `Impl`, the first is driven hot alone so
its memo entry is populated while the name is still unique, and only then does
the colliding class appear. Note it passes **with the epoch flush disabled as
well** — which is the honest reading of the paragraph above, not a claim that
the probe proves the epoch necessary. It proves the memo does not break
multi-loader dispatch, which is what it is for.

## Update 2026-07-31 — the scaling wall is fixed; the residual is the native-call floor

This section supersedes everything above it.

Three defects landed. Each was A/B'd on **one binary against itself** behind a
flag, which is the only comparison this document trusts (see the 2026-07-28
methodological note). Measured on the Windows box, 32 logical cores.

### 1. Every Java call serialized on process-wide counters

`probes/ScaleLadder.java` runs a ladder of increasingly VM-dependent
per-iteration operations at 1 and N threads, so a collapse can be attributed to
a *kind of operation* rather than to a benchmark arm. On `origin/dev` it showed
the wall directly: at ten threads a plain **virtual call went from 207 ns/op to
19,900**, i.e. ten threads doing a *tenth* of one thread's total work, while
pure arithmetic scaled 6x. Allocation, interface calls and `IdentityHashMap.get`
collapsed the same way; `getstatic` and primitive-returning natives did not.

Every interpreter/JIT boundary crossing did five contended global atomic RMWs
plus a global mutex, **twice** (once on enter, once on exit):

* `gc_quiescence::{ENTER_COUNT, LEAVE_COUNT}` — unconditional diagnostics,
* `gc_quiescence::JIT_ACTIVE_DEPTH` — a `fetch_update` CAS *loop* on the leave
  side, the worst possible shape under contention,
* `conservative_roots::GLOBAL_JIT_DEPTH`,
* `jit::ACTIVE_JIT_EXECUTIONS`,
* `types::jit_activation`'s single `Mutex<Registry>`, for both of its maps.

All of them are written on every call and read only by the GC, a diagnostic
dump, or code retirement. The four counters became
`cratonvm_types::striped_counter::StripedCounter` — one cache-line-aligned
stripe per thread, summed on the rare read. A thread's stripe never changes, so
`is_zero()` keeps exactly the contract `load() == 0` had, and per-stripe
saturation is strictly *safer*: one thread's unbalanced leave can no longer
cancel another thread's live entry.

`jit_activation` lost both halves rather than getting a cheaper lock. Its
`entry_ptr -> class_id` map was unnecessary — every activation site already
holds the `&CompiledMethod`, so the declaring class now travels on
`CompiledMethod::owner_class_id`, stamped at publication, which also cannot
return a retired entry's stale answer. Its `class_id -> count` map became a
per-thread chunked slot table: `enter`/`exit` touch only the calling thread's
own cache lines, and `active_class_ids` walks every thread's table from the
outside, which is sound because each published class id is an `AtomicU32`.

| ten-thread ns/op | legacy | striped + per-thread |
|---|---|---|
| virtual call | 8835 | **1493** |
| interface call | 8805 | **1728** |
| `IdentityHashMap.get` | 28375 | **1708** |
| allocation (`new TinyImpl`) | 8894 | **2199** |

`CRATONVM_STRIPED_COUNTERS_OFF=1` puts every thread back on stripe zero and
`CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX=1` restores the old lock.

### 2. `invokevirtual`/`invokeinterface` were wrongly excluded from RBC.6's escape hatch

`precise_exception_frame_sites_supported` did not admit `0xb6`/`0xb9`, so RBC.6
refused to compile any method whose handler reads a non-parameter local and
whose protected range calls one — **including `CharsetCache.getCharset`
itself**, whose `try` contains `invokevirtual addToCache`. Confirmed
mechanically, not by inference: with `CRATONVM_DBG_JIT_COMPILED=1` the method is
absent from the publication list under the narrow list and present under the
wide one.

They were removed in `a523715a84` because admitting them let Spring's
`SimpleApplicationEventMulticaster.invokeListener` read its own pre-`try`
`errorHandler` local back as null inside the handler. **That same commit also
added the fix**: the `protected_precise_handler_call` suppression in `x64.rs`,
which forces a protected virtual/interface call onto the dispatch path (ending
in `emit_post_invoke_exception_check`, which records the caller's complete
state) instead of the inline MIC/PIC cascade that machine-CALLs a raw entry
with nothing recording the caller's locals. The inline cascade was the actual
defect; the whitelist narrowing was belt to those braces. Both are in the tree,
so the opcodes are admitted again.

Acceptance: `probes/HandlerLocalAcrossProtectedInvokeProbe.java` builds the
Spring shape — a non-parameter local assigned before the `try`, read back in
the handler — through `invokevirtual`, `invokeinterface` and `invokespecial`
throw sites, plus a nested `try`, a reassignment after the protected call, and
(extended here) the null-`errorHandler` path that must skip the protected
region entirely, run 10,000 times *after* the branch has been profiled the
other way. It passes. `probes/Rbc6FieldProbe.java` is byte-identical to HotSpot
and to `--nojit`, whole-run checksum included, so `getfield`/`putfield` stay
out for the reason now recorded on `precise_frame_publishing_opcode`.
`cargo test -p cratonvm-jit` is green (1061 lib tests plus 11 integration
binaries) and `cargo test -p cratonvm-vm --test jit_local_exception_handler_tests`
is 15/15. The unit test that pinned the narrow contract now pins the wide one
and names the probe a future change has to re-verify with.

### 3. An exception-table callee cost a full dispatch on *every* call, forever

Compiling it was necessary but not sufficient — and this is the part every
earlier round of this document missed, because it is caller-side.

`probes/LazyArmVariants.java` decomposes the lazy arm one construct at a time.
Its V7/V8 pair is the decisive one: two delegates that differ **only** by a
`try`/`catch` that never fires, at the same call depth as the real arm (V0
against V2 changes two things at once, which is what hid this for so long).

| ten threads, ns/op | before | after |
|---|---|---|
| V7 delegate, no `try` (control) | 8362 | 8684 |
| V8 delegate, with `try` | 45613 | **10048** |
| V0 real `CharsetCache` | 46780 | **10055** |
| V2 no `try`/`catch` (control) | 9749 | 9973 |

A callee that declares an exception table is barred from the machine-code
MIC/PIC, because the inline cascade CALLs the cached entry directly. Nothing
ever writes the MIC's class id for such a callee, so every call landed in the
cache **miss** arm — a compile probe, an exception-table probe, and
`invoke_or_native` — and the compiled body sat in the JIT cache unused. Under
`--nojit` the V7/V8 gap disappears entirely, which is what proves the cost is
the compiled caller's dispatch and not the callee's body.

The virtual-MIC helper now keeps such callees in the per-thread
`VIRTUAL_DISPATCH_CACHE`, which the inline cascade cannot see, so a hit runs
the compiled body while still routing any `i64::MIN` sentinel through
`handle_compiled_callee_deopt_sentinel` — the same trade `jit_invoke_dispatch`'s
virtual arm already makes for its own callees. Both the entryless-hit and the
miss arm consult and populate it, and the helper now owes that cache the same
generation/supersede revalidation, factored out as
`flush_raw_entry_dispatch_caches`.

**The ban itself is obsolete, and is deliberately still on.**
`Compiler::emit_inline_callee_deopt_check` is emitted after *every* inline
direct-entry CALL (both PIC slots and the MIC arm) and hands a sentinel to
`jit_service_callee_deopt`, a thin wrapper over the same handler — so
publishing exception-table callees into the MIC/PIC would now be safe. Lifting
it was A/B'd on an idle host, three rounds, and is **indistinguishable** from
keeping it (V8: 9705/9779/9855 kept vs 8740/10229/9278 lifted), because the
Rust-level cache already captures the win. This document's own precedent
applies: a change carrying a correctness risk for zero measured throughput does
not land. `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1` opts in.

### What the whole change set is worth on the real class

**The real class, under JUnit, one run each** (`probes/run-doc23-regression.ps1`,
300s per-class timeout, same host, back to back, against `origin/dev`'s tip):

| arm | dev tip | this branch |
|---|---|---|
| `NoCsCache` (control) | 90.4s | 41.5s |
| `FullCsCache` | 76.4s | 62.9s |
| `LazyCsCache` | *still running at the 300s kill* | 49.9s |
| whole class | **TIMEOUT** | 154s, completes |

`dev` cannot finish the class inside 300s; this branch finishes it in 154s.
That is the headline: doc 23 opened on "the `LazyCsCache` arm alone takes 15
minutes, which is what pushes the class past even a 1500s per-class timeout",
and that part is over.

Note how much the two cached arms move against each other run to run — in this
one `LazyCsCache` (49.9s) came in *faster* than `FullCsCache` (62.9s), the
reverse of the earlier run against the same tree (54.5s vs 73.0s). They are
close enough now that their order is not stable, which is itself new: on dev
the lazy arm was 5x the full one.

The same three arms under `probes/Doc23Arms.java` at the class's own parameters
(10,000,000 iterations x 10 threads) on an idle host, where `dev` can be given
as long as it needs:

| arm | dev | this branch | change |
|---|---|---|---|
| `NoCsCache` (control) | 174.0s | 41.6-50.1s | **3.7x faster** |
| `FullCsCache` | 164.9s | 56.3-68.5s | **2.6x faster** |
| `LazyCsCache` | 459.5s | 75.5-78.3s | **5.9x faster** |
| `timeLazy / timeNone` | 2.64 | 1.50-1.81 | still FAILS |
| `timeFull / timeNone` | 0.95 | 1.12-1.65 | still FAILS |

Read the last row carefully, because it is counter-intuitive and the next
person will trip on it: **`timeFull` got worse as a ratio while getting 2.6x
faster in absolute terms.** The control arm is a single contended native
(`Charset.forName`), so it benefited *more* from the counter/lock work than the
cached arms did, and the ratio moved against us. Judging this class on ratios
alone, without the absolute times beside them, manufactures a "regression" that
is nothing of the sort. (Its own control-arm noise is separately documented in
the 2026-07-30 section: 8+ runs before any claim about `NoCsCache`.)

### What the test actually needs now

`probes/NativeCallCostProbe.java` measures each native the arms depend on
against `String.length()`, which establishes the floor for "call a native and
come back":

| | CratonVM 1t | CratonVM 10t | HotSpot 1t |
|---|---|---|---|
| *loop overhead alone, no call* (`ScaleLadder` arith) | *122* | *235* | *1.3* |
| `String.length()` — the floor | 749 | 6413 | 4.0 |
| `String.hashCode()` | 764 | 9116 | 4.2 |
| `toLowerCase(Locale)`, repeated receiver | 1296 | 13391 | 32.2 |
| `Map.get` on a `HashMap` | 822 | 3478 | 11.0 |
| `ConcurrentMap.get` on a CHM | 958 | 3406 | 13.8 |

Two things about that table, in order.

The first row is there so the rest are read correctly: this probe shape (a loop
inside a method entered once, so the loop reaches compiled code only through
OSR) costs ~122 ns/iteration before any call at all, which is itself ~90x
HotSpot and worth its own investigation.

And `String.length()` is **not a native** — CratonVM registers `length()` for
`StringBuilder` and `BitSet`, not for `String`, so under a real JDK it is
ordinary bytecode (`return value.length >> coder`). Its ~630 ns marginal cost
is therefore the price of a **call that is not inlined**, and the two natives
below it — both of which have thin direct helpers — land within 200-330 ns of
that same floor. So the label to carry forward is *call boundary*, not *native
dispatch*: the bodies are not the problem, and neither is native registration.
HotSpot's 4 ns on the same rows is not a fast call, it is *no call* — it
inlines all of them into the loop.

That marginal per-call cost is what the remaining ratio is made of:

* `NoCsCache` = **one** un-inlined call (`Charset.forName`).
* `FullCsCache` = **two** (`toLowerCase`, then `HashMap.get`).
* `LazyCsCache` = **two**, plus one more Java call level
  (`CharsetCache.getCharset`), and its map is a `ConcurrentHashMap`.

The arithmetic works out: `toLowerCase` (1296) + `HashMap.get` (822) = 2118
against `Charset.forName`'s 2290 — which is why `FullCsCache` and `NoCsCache`
now finish within ~10-60% of each other and the assertion turns on noise. On
HotSpot the floor is ~4 ns, so the *work* dominates and the caches win by 48x.
On CratonVM the floor dominates, so an arm making two calls cannot reliably
beat one making one, however good the caching is. No further work on the
charset cache, the dispatch helpers or the scaling counters changes that
ordering.

So this document's residual is the same one it has been deferring since
2026-07-30 as "the separate, already-known native-call dispatch overhead" —
except that it is broader than that name suggests, because the cheapest row
above is not a native at all. It is the cost of any call the JIT does not
inline. That is a VM-wide project, not a Tomcat one. Doc 23 stays OPEN and
should be closed by that work, not by more work on this test.

Ranked next steps, with the evidence for each:

1. **The per-call floor** (~630 ns/op marginal at one thread). `String.length()`
   in `NativeCallCostProbe` is the cheapest possible target — three bytecodes,
   no native, no charset fixture — and HotSpot spends ~0 on it because it
   inlines it. Worth roughly 2x on both cached arms and only 1x on the control,
   which is exactly the direction this test needs; it is what closes this
   document.
2. **`ConcurrentHashMap.get` versus `HashMap.get`.**
   `probes/ChmVsHashMapProbe.java` reaches one warm map through four declared
   types, because the thin direct helper is selected by the call site's
   *constant-pool class* — comparing a `Map`-typed field against a
   `ConcurrentMap`-typed one is an accidental apples-to-oranges test, and the
   earlier `ScaleProbe` `chmOnly`/`hashmapOnly` rows have exactly that flaw.
   On an idle host: `Map` to HashMap 345/2200 ns/op, `ConcurrentMap` to CHM
   1480/4400. Closing that gap is worth ~2.2 us/op to the lazy arm, more than
   the whole remaining `timeLazy` margin.
3. **The allocator.** `new int[4]` still scales at 0.47x (`ScaleLadder`) —
   `try_alloc_young_initialized` holds `young_from.lock()` across zeroing, and
   the JIT array path does not use the TLAB. It does not affect these arms (the
   run does **zero** collections, confirmed with `CRATONVM_GC_STATS=1`, so
   nothing here is GC-bound) but it is the next global lock of the same family
   as the ones fixed above.

### Reproduction

```powershell
# the three arms at any scale, with the two ratios the test asserts on
<cratonvm> --java-home <jdk25> -cp "<probes>;<tomcat classes>" Doc23Arms 10000000 10 1
# where multi-thread scaling collapses, by kind of operation
<cratonvm> --java-home <jdk25> -cp <probes> ScaleLadder 1000000 10
# the lazy arm one construct at a time (V7 against V8 is the try/catch pair)
<cratonvm> --java-home <jdk25> -cp "<probes>;<tomcat classes>" LazyArmVariants 300000 10
# the native-call floor
<cratonvm> --java-home <jdk25> -cp <probes> NativeCallCostProbe 500000 10
# CHM against HashMap without the declared-type confound
<cratonvm> --java-home <jdk25> -cp <probes> ChmVsHashMapProbe 500000 10
# did my flag move anything? -- always answer this before measuring
CRATONVM_DBG_JIT_COMPILED=1 <cratonvm> ... 2>&1 | Select-String getCharset
```

Regression check for the change set (`probes/run-doc23-regression.ps1`): of the
63 `org.apache.tomcat.util.{buf,collections,http}` / `org.apache.catalina.util`
classes, **62 give an identical verdict** on `origin/dev`'s tip and on this
branch merged on top of it — including the same two `*LargeHeap` failures (they
want more than the 2g used here) and the same `TestMethodPerformance` timeout.
The single class that differs is `TestCharsetCachePerformance` itself, which
goes from TIMEOUT to a completed FAIL.

Rust-side: `cargo test` is green for `cratonvm-jit`, `cratonvm-types` and
`cratonvm-gc` (28 result groups, 0 failures), and
`cargo test -p cratonvm-vm --test jit_local_exception_handler_tests` is 15/15.
`cargo test -p cratonvm-vm` has two failures —
`env_cache::tests::no_presence_predicate_shadows_a_compound_flag_default` and
`serviceability::tests::obsaudit_attach_listener_creates_a_real_socket` — which
were checked out and reproduced on **plain `origin/dev`** with identical
messages, so they are pre-existing and unrelated to this change set.
