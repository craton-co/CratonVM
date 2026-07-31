# Three conservative JIT-admission bans leave `TestMethodPerformance`'s whole hot path interpreted

**Status:** ✅ **CLOSED 2026-07-31.** Every admission ban this document names is
settled, the "next lever" its last update identified is implemented, and the
residual throughput gap has been re-derived from measurement and re-homed. It
is not an admission-ban problem, and has not been one since ban 1 cleared.

Residual of [24](24-stringcache-oom-under-load-FIXED.md) (whose
`OutOfMemoryError` is FIXED). Family of
[31](31-synchronized-code-never-jit-compiled-FIXED.md), and of the retired
[04](04-embedded-server-throughput-wall-CLOSED.md) /
[29](29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md).

> **Read this before chasing the number.** The headline "730x on the class" was
> real, but its stated cause was wrong. With all three bans settled, loop
> control in this shape runs at **HotSpot parity** — 1 ns/op against HotSpot's
> 2 — and so does arithmetic. What remains is per-frame and native-call cost in
> the JDK charset chain that `MessageBytes.toStringType` sits on top of. That
> is a VM-wide baseline issue; it is measured and re-homed in
> [§ Where the time actually goes](#where-the-time-actually-goes-re-derived-2026-07-31).

---

## Symptom (as originally recorded)

`org.apache.tomcat.util.http.TestMethodPerformance` runs 6 × 100 000 000
iterations of `mb.setBytes(...); mb.toStringType();` and then 6 × 100 000 000
of `Method.bytesToString(...)`. HotSpot finishes the class in **41.2 s**.

Measured on the post-[24](24-stringcache-oom-under-load-FIXED.md) binary, from
the class's own printout:

```
.MessageBytes conversion took :3820342393100ns      (CratonVM, 1st 100M loop)
MessageBytes conversion took :3092470156300ns       (CratonVM, 2nd 100M loop)
MessageBytes conversion took :6573830400ns          (HotSpot, same loop)
```

Run to completion the class **PASSES**, in **30 149 s (8.4 hours)**. Nothing
here was ever a functional defect; it is purely throughput. Before the bug-24
fix this was masked — the run died with a spurious OOM at ~150-600 s.

**Re-measured 2026-07-31, and the class-level number has NOT moved:**

```
.MessageBytes conversion took :4075206863800ns     (100M iterations, 40.75 us/iter)
```

against the original 3820342393100 ns (38.2 µs/iter). That is the expected
result, not a disappointment — it is exactly what the per-iteration
decomposition below predicts. The admission bans governed *loop control*, which
is now at HotSpot parity; the ~38 µs an iteration costs is spent in the JDK
charset chain the loop body calls into, and no admission gate was ever standing
in front of that. **Anyone re-opening this document because "the number didn't
change" should read
[§ Where the time actually goes](#where-the-time-actually-goes-re-derived-2026-07-31)
first.**

(That loop ran while the 646-class A/B below was saturating the box, so treat
40.75 µs as an upper bound; the quiet-host per-iteration figure is ~38 µs.)

## The three bans, and how each ended

| # | Ban | Disposition |
|---|-----|-------------|
| 1 | **RBC.7** — `compile_osr_artifact` refuses any method containing `invokedynamic` | ✅ premise removed 2026-07-30 (concat sites are bridged, not trapped) |
| 1b | **`osr_dead_mask`** — a second, independent gate hidden behind ban 1 | ✅ **FIXED here** — see below |
| 2 | **RBC.6** — `local_handler_reads_unsafe_local` refuses `StringCache.toString` | ⚠️ **still refuses it, deliberately** — this doc's earlier "lifted" claim is stale, see below |
| 3 | **Constructor** — `classify_init_complexity` refuses any `<init>` containing `putfield` | ✅ lifted by default 2026-07-28, regression-settled here |

### Ban 1 — cleared, and it was hiding a second gate

`compile_osr_artifact`'s blanket `if !scan.indy_ops.is_empty() { return None; }`
refused `testGetMethodPerformance` because the
`System.out.println("..." + duration + "ns")` after each loop lowers to
`invokedynamic StringConcatFactory`. Since the test method is a once-invoked
harness method with the hot loop inline, OSR is the only route into compiled
code, so all 600 000 000 iterations of loop control interpreted.

That ban's premise was removed on 2026-07-30 by lowering a resolved
`StringConcatFactory` site to a direct call instead of an uncommon trap
(`make_jit_string_concat_site_from_parts` / `execute_jit_string_concat_raw`,
the `0xba` arm in `jit/src/x64.rs`). RBC.7 now only covers *unbridged*
bootstraps.

**And the loop still did not OSR**, because a completely independent gate
refused the same entry and had been invisible for as long as RBC.7 bailed
first:

```
[cratonvm-jitc] indy-concat bridge pc=25 args=1          <- RBC.7 passed
[cratonvm-jitc] OSR-reject OsrConcatProbe.main([Ljava/lang/String;)V
                entry_pc=4 (dead_mask non-zero; memoed)
```

`CompiledMethod::can_osr_enter` refused any entry pc whose `osr_dead_mask` is
non-zero. At `entry_pc=4` the mask is `0x1` — local 0 (`args`), dead at the
loop head but register-resident and sharing a home GPR with a live local.

### Ban 1b — the `osr_dead_mask` refusal, FIXED

The previous revision of this document concluded:

> The root problem is that `osr_local_assignments` is a **whole-method** table:
> it cannot express "at this pc this register belongs to local *j*, not local
> *i*". … Making this entry safe means giving OSR a per-pc local→location map,
> not relaxing the check.

**That conclusion was wrong.** No per-pc map is needed, because the locations
are already per-method and the allocator's own invariant closes the gap:

1. **A local has exactly one home for the whole method.** `local_assignments`
   is indexed by local, `x64::Compiler::reg_for_local` is its single reader,
   and there is no live-range splitting — so there is no "at this pc register R
   belongs to someone else" state to express in the first place. The OSR copy
   only *nulls* entries (category-2 high halves); it never re-points them.
2. **Two locals sharing a register are never both live-in at the same block
   start.** `regalloc::build_interference` unions every block's `live_in`
   against itself, so any pair simultaneously live-in at a block boundary is
   marked interfering — and `regalloc_invariants_hold` throws the entire
   allocation away (falling back to no register homes) if an interfering pair
   received the same colour. `RegAllocResult::block_live_in`, which the mask is
   computed from, is that *same* `blocks[i].live_in`.
3. Therefore at an OSR entry pc — always a block start — **at most one of the
   locals homed to register R is live**, and the trampoline, which skips
   exactly the masked ones, loads that one. R ends up holding the correct
   value.
4. A masked local needs no value: dead means every path from the entry
   redefines it before reading it. Its frame slot goes unwritten, but the
   trampoline already elides the frame-slot store for *every* register-homed
   local, dead or live, so that is not a new hole.

The refusal (`3415d052b`, 2026-07-03) also turns out to have landed alongside
the change that actually closed the Hibernate regression it cites: the same
commit threaded `compute_param_jvm_slots` / `param_slot_span` into the OSR
compile so category-2 parameters — the `long limitRows` in that very
`org.h2.command.query.Select.queryFlat` frame — land in the slots the body
reads. That mismatch, not the dead-local skip, produced the NPE.

Lifted by default; **`CRATONVM_JIT_OSR_DEAD_LOCALS=0` restores the blanket
refusal with no rebuild**. `can_osr_enter_with` is the pure variant so one
process can pin both sides.

**Result.** `OsrConcatProbe.main` now logs `OSR-compile entry_pc=4` followed by
`OSR-reuse` where it logged `OSR-reject`, with `first`/`second` unchanged and
matching HotSpot. Same on the real shape (`OsrMessageBytesProbe`,
`entry_pc=24`). Interleaved A/B, one binary one knob apart, 500k iterations:

| round | admitted (ns) | refused (ns) |
|---|---|---|
| 1 | 12 942 854 700 | 14 134 067 700 |
| 2 | 11 927 264 500 | 13 676 509 100 |
| 3 | 12 139 174 800 | 14 197 219 100 |
| **mean** | **12 336 431 333** | **14 002 598 633** |

**~12% faster** (wall 13.49 s vs 14.84 s). Modest, and that is the point: see
the re-derivation below for why.

#### It found a real miscompile on the way

The differential written for this change (`probes/OsrDeadLocalProbe.java`,
eight coalescing shapes, FNV-checksummed) failed on its first run — and the
failure reproduced with `CRATONVM_JIT_OSR_DEAD_LOCALS=0`, i.e. **on plain
`dev`**, independent of this change. Narrowed to `probes/SlotReuseCategoryProbe.java`:

```
                       HotSpot / --nojit   CratonVM JIT
reused                        5599982.5    1.7716133435E9   <-- wrong
noReuse                       5599982.5    5599982.5
intAfter                      5599982.5    5599982.5
noAfter                       5599982.5    5599982.5
dblThenIntCounter             5599982.5    5599982.5
```

javac reuses a JVM local slot the moment the previous variable's scope ends and
does not care whether the next occupant shares its type category:

```java
for (int i = 0; i < N; i++) { sum += (i % 17) * 0.5 + acc0; }
double tail = sum / 3.0;      // javac gives `tail` the slot `i` had
```

`find_float_locals` marks that slot float on the strength of the `dstore`, so
it gets an XMM home for the whole method, while its `iload`/`istore`/`iinc` go
through the canonical frame slot (`reg_for_local` returns `None` for a
float-masked local). Each category is internally consistent, so ordinary entry
compiles correct code — but the OSR trampoline seeds locals by index and elides
the frame-slot store for any local with a register home. The interpreter's
`int i` lands in an XMM nothing reads, and the frame slot the loop actually
reads is left **uninitialised**: the counter starts at garbage.

Fixed in `86e5122b5`: `find_non_float_locals` scans the GPR-category accesses,
and a slot in both scans gets neither a GPR nor an XMM home, so both categories
use the frame slot and the trampoline seeds it. This costs nothing on the int
side — such slots already had no GPR home. The sibling shape (a category-2's
dead high half reused for a real local, `probes/HighHalfReuseProbe.java`) does
**not** reproduce, so `wide_local_high_halves` was left alone.

> **On the regression gate for this one.** The deterministic gate is the
> allocator unit test `regalloc::tests::dual_category_slot_gets_no_register_home`,
> which pins the decision directly (`assignments[2]` and `xmm_assignments[2]`
> both `None` for a slot used as both `int` and `double`) and fails on a
> reverted fix every time.
>
> An **end-to-end** Java fixture was written first and then withdrawn, and the
> reason is worth recording. The defect only bites on a round that actually
> OSR-*enters*, and from Java there is no way to force that: on the un-fixed
> tree the fixture caught it in **1 run out of 3** under `cargo test`'s default
> parallelism, and its very first single-round version passed under default
> parallelism while failing under `--test-threads=1`. A test that silently
> passes two times in three on broken code is worse than no test, because it
> reads as coverage. `probes/SlotReuseCategoryProbe.java` remains the
> end-to-end reproduction, run by hand against a real binary.

This is the third time a "just throughput" Tomcat doc has turned out to contain
a real correctness defect (cf. 29, 32). Re-derive them item by item.

### Ban 2 — RBC.6 still refuses `StringCache.toString`, on purpose

**This document's "Update 2026-07-27 — ban 2 (RBC.6) is lifted" is stale and is
retained below only as history.** The widening of
`precise_exception_frame_sites_supported` to `0xb6` / `0xb9` was reverted on
`dev`:

* `a523715a8` (2026-07-29) removed them, because they let Spring's
  `SimpleApplicationEventMulticaster.invokeListener` read its own pre-try local
  back as **null** inside the handler; and
* `cd50a2208` (2026-07-30) reverted a re-widening attempt, recording the
  measurement that settles it: *"A/B'd on one binary one line apart: the
  widening is worth nothing to this test now that try/catch methods reach the
  optimizing tier by other means."*

Verified live on this tree — `StringCache.toString` is still refused:

```
[cratonvm-jitc] resolver-bail site=rbc6-handler-reads-unsafe-local
    org/apache/tomcat/util/buf/StringCache.toString(...)Ljava/lang/String;
[cratonvm-jitc] compile-bail ... backend_attempted=false
```

**Do not re-widen `0xb6`/`0xb9` to chase this doc.** It has been tried twice,
it costs a silent wrong-locals defect in Spring, and it is measurably worth
nothing here. `StringCache.toString` is the *only* method in the per-iteration
chain that does not compile — every one of its neighbours does:

| method | status |
|---|---|
| `MessageBytes.toStringType()` | ✅ C1, then C2 |
| `ByteChunk.toString()` | ✅ full-compile |
| `ByteChunk.toString(a, b)` | ✅ C1 full-compile |
| **`StringCache.toString(bc, a, b)`** | ❌ RBC.6 resolver-bail |
| `ByteChunk.toStringInternal(a, b)` | ✅ C1 full-compile |

Its measured share is ~4 µs of a ~38 µs iteration (~10%). The canonical
analysis of this gate is [23](../../../known-issues/tomcat/23-charsetcache-pathological-slowdown.md),
which remains OPEN on its own residual (a thread-scaling wall in the dispatch
helper, not an admission question).

### Ban 3 — the constructor `putfield` ban

`classify_init_complexity` (`vm/src/jit/skip_list.rs`) marked any `<init>`
containing `putfield`, `putstatic`, `monitorenter/exit` or `invokedynamic` as
`InitComplexity::Complex`, and `should_skip_jit_with_init` refused it with
`SkipReason::Constructor` — so a constructor that assigns a field, which is
what constructors are *for*, was never compiled and, unlike the RBC.6 case,
never even **enqueued**. That was a VM-wide ceiling on allocation.

**Lifted by default 2026-07-28**, for `putfield`-only constructors;
`putstatic` / `monitorenter-exit` / `invokedynamic` constructors remain banned.
Kill switch `CRATONVM_JIT_PUTFIELD_INIT=0`. Measured prize: `allocBody` (a
constructor assigning one field) **1846 ns → 226 ns, 8.2×**, with the
empty-constructor controls flat.

The outstanding item was the regression run. The Tomcat arm of it is **done**
— see [§ Regression evidence](#regression-evidence-2026-07-31) — and is clean.

**The knob is kept, deliberately, and the previous revision's instruction to
delete it is not followed.** That instruction ("if it comes back clean, delete
the `putfield` arm from `classify_init_complexity` outright and retire the
knob") was written before the knob had been used for anything. Two things
changed since:

* **It is the mechanism by which this document's own regression evidence was
  produced.** Every A/B above is one binary with `CRATONVM_JIT_PUTFIELD_INIT=0`
  on one side. Deleting the knob deletes the ability to run that comparison
  again on a shipped binary, which is exactly what someone chasing a future
  miscompile will want first.
* **The stated bar is only partly met.** The bar was "Tomcat + Spring Boot +
  Hibernate on a quiet host". Tomcat is done here. Spring Boot and Hibernate
  are not, and this ban still carries no incident write-up — it predates the
  open-source import (`a6dc911ed`) and rests on a one-line "field stores
  trigger the JIT's load-forwarding interaction" note. Removing the escape
  hatch while two thirds of the evidence is outstanding trades a real option
  for a cosmetic cleanup.

Retire the knob when the Spring Boot and Hibernate arms are also clean. Until
then the ban is **settled as default-lifted**, which is what this document
needed; the knob's continued existence is not an open issue.

## Where the time actually goes (re-derived 2026-07-31)

`probes/MbChainCostProbe.java` decomposes the per-iteration chain into the
frames it is actually made of. Every stage is a plain static method with the
loop **inline** — an earlier revision drove the stages through a `Runnable` and
measured ~3.4 µs per call on CratonVM, which buried everything it was meant to
compare. Six blocks of 100 000; steady-state block, ns/op:

| stage | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `emptyLoop` | 2 | **1** | **parity** |
| `setBytesOnly` | 3 | 759 | 253× |
| `byteBufferWrap` | 1 | 1 304 | — (HotSpot escape-analyses it away) |
| `newStringFromChars` | 7 | 2 000 | 285× |
| `charsetDecode` | 28 | 4 600 | 164× |
| `inlineEquivalent` (decode + `new String`, inline) | 54 | 16 503 | 305× |
| `toStringInternalDirect` | 50 | 21 824 | 436× |
| `stringCacheDirect` | 53 | 25 870 | 488× |
| `byteChunkToString` | 29 | 33 883 | 1168× |
| `mbFullChain` | 28 | ~38 000 | ~1350× |

And `probes/AllocScalingProbe.java`, ten blocks of 200 000:

| shape | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `arithOnly` | 2 | **1** | **parity** |
| `newIntArray8` | 4 | 109 | 27× |
| `newCharArray3` | 5 | 101 | 20× |
| `newSmallObject` | 4 | 260 | 65× |
| `newStringFromChars` | 11 | 680 | 62× |

Read those two tables together and the doc's original thesis collapses:

* **Loop control and arithmetic are at parity.** `emptyLoop` 1 ns, `arithOnly`
  1 ns. Whatever the 730× is, it is not interpreted loop control — which is
  precisely what all three admission bans were about.

  That row is also the cleanest positive evidence that the ban-1b lift works.
  `emptyLoop` is invoked **6 times** (once per block), far below the
  500-invocation whole-method threshold, so it cannot have been compiled by
  tier-up. Running it at 1 ns/op means its internal loop was **OSR-entered** —
  the exact entry `can_osr_enter` used to refuse.
* **Allocation is 20-65×, and flat.** It does not degrade with heap occupancy,
  and `-Xlog:gc*=info` reports `minor=7 major=0` across the whole probe, so
  this is not a GC story either.
* **The cost is per-frame, in this specific chain.** Each additional Java frame
  between `setBytes` and the decode adds 4-8 µs, and the decode itself is
  ~4.6 µs against HotSpot's 28 ns.

### The residual, named as precisely as this document can name it

`probes/CharsetDecodeShapeProbe.java` splits `Charset.decode`'s cost into its
fixed per-call and marginal per-byte parts, by decoding payloads of 3, 30, 300
and 3000 bytes in one process. (Run while the full-suite A/B was occupying the
box, so the absolute figures are roughly 2× inflated against the quiet-host
numbers above — the *shape* is the point, and every row shares the same load.)

| payload | CratonVM ns/call | HotSpot ns/call | CratonVM `ByteBuffer.wrap` alone |
|---|---|---|---|
| 3 B | 12 017 | 99 | 3 005 |
| 30 B | 8 751 | 260 | 2 984 |
| 300 B | 24 436 | 256 | 1 889 |
| 3000 B | 104 811 | 1 400 | 2 077 |

Two separate terms fall out:

* **A fixed per-call cost of roughly 8-12 µs** against HotSpot's ~250 ns — and
  ~2-3 µs of that is `ByteBuffer.wrap` on its own, before any charset work
  happens. For this document's workload (**3 bytes**) the fixed term is
  essentially the entire cost.
* **A marginal transcoding cost of ~30 ns/byte** (from the 300 → 3000 step)
  against HotSpot's ~0.42 — a separate ~70×, which only matters for large
  payloads. `TestMethodPerformance` does not have them.

One concrete but **unverified** lead for the fixed term, recorded for whoever
picks it up: `native_charset_decode_bytebuf` → `decode_with_charset`
(`native-builtins/src/charset.rs`) reads the `Charset` object's `name` String
out of the heap, converts it to a Rust `String`, runs it through
`normalize_charset_name` (a second allocation), and then dispatches into the
transcoding engine **by name string** — on every call, independent of payload.
`encode_with_charset` does the same. Memoising the resolved charset on the
`Charset` object's identity is the obvious move and is deliberately **not**
attempted here: this tree has been bitten twice by exactly that shape
(`reference_get_field_by_name_memo_is_a_slowdown` — a memo that cost 37% — and
`reference_per_thread_memo_bypasses_slowpath_filter` — a memo in front of a
fast path that skipped its post-lookup filters). It needs its own quiet-host
A/B, which is a perf project rather than a known-issue closeout.

So the residual belongs to the VM-wide native-call / dispatch baseline —
`reference_native_call_direct_helper_vs_generic_dispatch_35x`,
`reference_junit5_execution_machinery_dispatch_overhead`, and the half-gap perf
projects — not to this document. **Re-homed there.**

> **Two measurement traps this re-derivation walked into**, recorded so the
> next person does not:
>
> 1. Running each stage through a `Runnable`/`IntConsumer` made every row read
>    ~3.4 µs, because that *is* what a lambda interface call costs here. Inline
>    the loop into a named static method.
> 2. A first pass showed `charsetDecode` growing 5090 → 10249 ns across blocks,
>    and separate processes showed 15.5 µs/iter at 100k against 35.3 µs at
>    800k. **Neither reproduced.** A repeat gave 4444 → 4629, flat. There is no
>    superlinear trend; it was host noise. Repeat the baseline.

## Regression evidence (2026-07-31)

All on one binary, both relaxations toggled together against their historical
behaviour — arm A = defaults (`osr_dead_mask` entry admitted, `putfield`-ctor
ban lifted), arm B = `CRATONVM_JIT_OSR_DEAD_LOCALS=0` +
`CRATONVM_JIT_PUTFIELD_INIT=0`. One binary, two knobs, nothing else differs.

* **`cargo test -p cratonvm-jit`** — green: **1064** lib tests + **193** across
  the 11 integration binaries, 0 failures. (This suite was reported
  uncompilable on `dev` in the previous revision — `E0063: missing fields
  service_callee_deopt and set_throw_bci`. That was fixed on `dev` in the
  interim; verified here.)
* **`probes/OsrDeadLocalProbe.java`** — HotSpot, CratonVM JIT, CratonVM with
  the kill switch, `--nojit`, and `CRATONVM_JIT_OSR_DEAD_MASK_BLANKET=1` all
  return `acc=5697627218349681645` at 400k iterations and
  `acc=-3383397992731040631` at 3M.
* **`probes/SlotReuseCategoryProbe.java`**, **`probes/HighHalfReuseProbe.java`**,
  **`probes/OsrDoubleShapeProbe.java`** — all arms identical to HotSpot.
* **Tomcat `util.{buf,collections,http}` / `catalina.util`** (the same 63-class
  set `cd50a2208` used): **55 classes completed, 0 status differences**. The
  non-PASS classes are identical on both sides — `TestByteChunkLargeHeap` and
  `TestCharChunkLargeHeap` FAIL on both (known LargeHeap fixture gap),
  `TestCharsetCachePerformance` HANGs on both (doc 23's known-slow class).
* **Full Tomcat suite, 646 classes, both arms run concurrently** so they saw
  the same host load:

  | | PASS | FAIL | HANG | NOSUMMARY | wall |
  |---|---|---|---|---|---|
  | **A** (defaults) | 560 | 47 | 38 | 1 | 143.8 min |
  | **B** (both knobs restored) | 558 | 46 | 40 | 2 | 145.9 min |

  **638 of 646 classes (98.8%) have identical status.** The 8 that differ are
  bidirectional — arm A is worse on 3 and arm B on 5 — which is the signature
  of flake, not of a change that breaks things. Each was then re-run
  individually on a quiet box at a 900 s timeout (`repeat-diffs.ps1`), and
  **every one passes in BOTH arms**:

  | class | in-suite A / B | repeat A | repeat B |
  |---|---|---|---|
  | `TestHttpServletDoHeadInvalidWrite0ValidWrite1` | FAIL / PASS | PASS ×3 | PASS ×3 |
  | `TestHttpServletDoHeadInvalidWrite1ValidWrite1` | FAIL / PASS | PASS | PASS |
  | `TestHttpServletDoHeadInvalidWrite1023ValidWrite512` | PASS / FAIL | PASS | PASS |
  | `tribes…TestTcpFailureDetector` | FAIL / PASS | PASS | PASS |
  | `tribes…TestNonBlockingCoordinator` | PASS / HANG | PASS | PASS |
  | `coyote.http11.TestHttp11InputBuffer` | PASS / NOSUMMARY | PASS | PASS |
  | `coyote.http2.TestAsyncFlush` | PASS / FAIL | PASS | PASS |
  | `jasper.compiler.TestCompiler` | PASS / HANG | PASS (148 s) | PASS (142 s) |

  So **nothing in the 646-class suite is attributable to either knob.** The
  `TestHttpServletDoHead*` family in particular ran 67-99 s standalone against
  a 300 s in-suite timeout — the documented straddle — and all 8 sit in
  families `00-INDEX.md` already records as environmental (that timing
  straddle, Tribes multicast, HTTP/2).

> **Warning to anyone measuring this.** `TestDefaultServlet` has a
> **pre-existing flaky stack overflow** on `dev` under load — an unmodified
> baseline binary crashed once in four runs with "thread 'main-vm' has
> overflowed its stack" while this host ran three concurrent heavy jobs. It ate
> an entire investigation cycle: a single crash-vs-pass pair was read as
> attribution three separate times, and every one was refuted by simply
> repeating the baseline. **Repeat the control before believing any difference
> against this class.**

## Appendix — history retained

### Original supporting measurements (isolated probes, multitenant host)

> Absolute rates are a lower bound — this Windows box was multitenant
> throughout. The structural findings are compile-time facts read out of a
> trace, so they do not depend on host load.

| probe | what it does | HotSpot | CratonVM |
|---|---|---|---|
| `arith` | no call, no allocation | ~0 | 1 |
| `scall` | one `invokestatic` | ~0 | 7 |
| `vcall` | one `invokevirtual` | ~0 | 56 |
| `pcall` | one `invokespecial` (private method) | – | 60 |
| `directSet` | `putfield` on an old receiver | – | 54 |
| `allocArr` | `new int[1]` | – | 126 |
| `allocNoCtor` | `new Plain()` (empty ctor) | – | 162 |
| `allocArg` | ctor takes an arg, empty body | – | 278 |
| `allocNEsc` | allocation inside a **C1**-compiled callee | 3 | 105 |
| `allocEsc` | same allocation **inline in the C2/OSR loop** | 3 | 2331 |
| `allocBody` | `new Body()` whose ctor writes one field | – | 2338 |

Both leads out of that table were root-caused: `allocBody`'s 20× became ban 3,
and the endless OSR recompile loop was fixed (`allocPutOld` logged **200
`OSR-compile` events for 200 000 iterations**; the `OSR-recompile reason=`
trace attributed 199 to `cached-cannot-enter-at-pc`. The back-edge path treated
"published artifact that cannot be entered at this pc" as *still compiling* and
re-requested forever, instead of consuming the bounded per-pc rejection budget.
Now 200 → **1**).

### Update 2026-07-27 — ban 2 lifted (SUPERSEDED, see § Ban 2)

`precise_exception_frame_sites_supported` admitted `invokevirtual` /
`invokespecial` / `invokeinterface` alongside `invokestatic` and the monitor
ops. **Reverted on `dev` in `a523715a8` / `cd50a2208`** — this update no longer
describes the tree.

### Update 2026-07-30 — ban 1's premise removed, second gate found

See § Ban 1 and § Ban 1b above. Correctness evidence for the concat bridge:
`ConcatBridgeProbe` drives 14 distinct `makeConcatWithConstants` shapes from a
hot loop and FNV-checksums every string; HotSpot JDK 25, CratonVM JIT and
CratonVM `--nojit` all give `510489415044571348`. `OsrConcatProbe` returns
`first=12499997500000 second=24999995000000`, matching HotSpot — no duplicate
loop execution. Five Tomcat classes A/B'd against pure `origin/dev` were
identical.

### Deliberately NOT ported from the handover branch

The originating worktree (`codex/fix-tomcat-hotloop-jit-admission-20260728`,
based 187 commits behind) also carried three changes that were dropped:

1. **The RBC.6 admission-gate rewrite.** It deleted the `return false` in
   `precise_exception_frame_sites_supported`, leaving an empty `if` whose
   comment still claims it checks something — i.e. the gate admits everything.
   Subsequent history (§ Ban 2) vindicates the decision to drop it.
2. **The generic protected-range exact-resume trap** in `x64.rs`, which existed
   only to justify (1).
3. **`RETIRED_COMPILED_METHODS`** — process-lifetime retention of every
   superseded compiled body. `dev` already solves that with `defer_jit_owner`.

`DIRECT_CALLEE_EXCEPTION_ROUTE_TAG` was also left out — an independent
optimisation, not part of ban 1.

## Reproduction

```bash
CP=$(cat apps/tomcat/.suite/cp.txt)
CRATONVM_DBG=jitc <cratonvm.exe> -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance \
  2>&1 | grep -iE 'TestMethodPerformance|StringCache|OSR-'
```

For the decomposition rather than the class:

```bash
<cratonvm.exe> -Xmx2g -cp "probes/out;$CP" MbChainCostProbe 6 100000
```
