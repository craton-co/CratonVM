# ✅ FIXED — every JIT `getfield` took the checked helper, on every collector

**CLOSED 2026-08-20.** Both actionable defects are fixed, the follow-on walk in
the native-dispatch path is fixed and priced, and the one residual left is not a
getfield problem at all — it is the ZGC coloured-slot representation, tracked
where the fix for it lives: `docs/feature-designs/zgc-jit-load-barrier.md` §
"The getfield residual this blocks".

| # | defect | closed |
|---|---|---|
| 2 | **Generational** — `legacy-layout-receiver`: 68 722 450 helper calls -> **0**, 3.07x | 2026-08-18 |
| 1 | **G1** — `outside-published-bounds`: 56 930 918 helper calls -> **0**, wall clock 1.33x-2.14x over 6 of 6 pinned interleaved pairs | 2026-08-18 |
| — | **ZGC**, part 1 — stop asking `is_object_address` on a receiver the IR already types `Ref`: 34 470 791 of 34 470 791 helper calls take the proven-oop path, ~1.05x | 2026-08-18 |
| — | **ZGC**, part 2 — validate ONCE per native accessor call: 64 248 919 -> 38 879 898 walks, **1.07x** on ZGC and **1.11x** on Generational, 5 of 5 interleaved pairs each | 2026-08-20 |

On all three collectors the inline path is engaged for **every primitive field
read** — 0 primitive misses, measured two independent ways — and the entire
remainder is **reference** reads.

**What is NOT closed, and is not this page's to close.** ZGC still takes the
helper for reference reads: 56.9M calls, 100% `outside-published-bounds`, and
deliberately so. A compact reference slot there holds
`Z_COLORED_TAG | colour | offset`, not a pointer, so inlining its load is the
use-after-free that `zgc-jit-load-barrier.md` exists to prevent, and ZGC
publishes nothing into the read table for exactly that reason. That is a
property of the slot representation, not of the getfield arms, and it is
recorded in the design doc so it is found by whoever implements the barrier
rather than by whoever greps known-issues.

**The instrument that found most of this is retired.** The whole-VM
`#[track_caller]` per-caller census in `gc/src/vm_heap.rs` is gone: it cost an
atomic plus an open-addressed probe on every membership walk, which inflated
every ZGC number on this page by 3.4x before that was caught. `--diag` still
prints the hand-tagged JIT-site census. See "The census that found this is
retired" below for what that costs and where to find it again.

**Read the corrections.** This page was wrong three times in ways that each cost
a session — the title twice, and a whole round of numbers once. The three
CORRECTION sections are the most transferable part of it and are the reason the
body is kept in full rather than summarised.

---

Everything below is the page as it stood, including every hypothesis that was
refuted along the way.

---

# Every JIT `getfield` takes the checked helper — TWO independent guard clauses fail, one per collector family

## Status
**FIXED on every collector. ZGC's residual closed 2026-08-19; the design it was
said to be blocked on turned out to describe a tree that does not exist yet.**
Three defects, one per collector family, all closed:

* **Generational** (`legacy-layout-receiver`, defect 2) — closed 2026-08-18 on
  the reader side: 68 722 450 helper calls -> **0**, 3.07x.
* **G1** (`outside-published-bounds`, defect 1) — closed 2026-08-18 by the
  READ-side bounds table item 2 below specifies: **56 930 918 helper calls ->
  0**. Wall clock improved in 6 of 6 pinned interleaved pairs, 1.33x-2.14x; see
  item 2 for why that is quoted as a range and why the first version of this
  line said 2.35x.
* **ZGC** — **CLOSED 2026-08-19.** The engagement counter reads a literal zero
  on the default collector: `probes/AccessorDispatchProbe.java` at 3 rounds x
  2 000 000 goes **17.8-18.0 M helper calls → 0** across three interleaved
  reps, and `probes/BobyqaOne.java` goes **2 125 738 → 0**. `receiverFieldTax`
  falls 14.7/15.2/17.8 ns → 5.1/9.3/5.4 ns on a loaded host.

  This bullet used to read "still 56.9M, and deliberately so", because a
  compact reference slot under ZGC was said to hold
  `Z_COLORED_TAG | colour | offset` rather than a pointer. **That premise is
  true of the relocating ZGC `feature-designs/zgc-jit-load-barrier.md` designs
  and false of the one that runs.**
  `feature-designs/zgc-reference-slot-representation.md` opens with the
  measured position — *"Reference slots are plain pointers; nothing in the heap
  stores a colored word"* — and `ZgcRealHeap::set_barrier_color`, the sole
  writer of the coloured state, has **no non-test caller**, so the barrier this
  residual was waiting on is never armed in a real process.

  So ZGC publishes its arena envelope, and the publish is COUPLED to the
  barrier rather than betting on it: arming CLEARS `JIT_READ_BOUNDS`, disarming
  refills it. That is strictly stronger than the emission-time gate it joins,
  because the emitted containment sequence reads the table at runtime — an
  empty table disarms sequences that are ALREADY COMPILED, which
  `narrow_oops_block_inline_fields` cannot reach. Default ON,
  `CRATONVM_ZGC_NO_JIT_READ_BOUNDS=1` restores helper-only reads.

  **What it is worth, stated honestly.** On a field-dense kernel it is the whole
  residual. On `BobyqaOne` it is ~1.08x of wall clock and no more, because
  2.1 M helper calls at ~10 ns is 21 ms of a 25-second run — that workload's
  real defect was an OSR admission failure, recorded in
  fixed-bugs/osr-entry-deferred-to-an-oop-mask-that-never-ran-FIXED-20260819.md.
  A count going to zero is not by itself a speedup of anything in particular.

On all three collectors the inline path is engaged for **every primitive field
read** — 0 primitive misses, measured two independent ways — and the entire
remainder is **reference** reads.

This title has been wrong twice and is now half-wrong a third time: it says
"every" and "always", and after 2026-08-18 that is true only on ZGC. Left as
written because it is the string people search for; the Status block is the
authority.

This title has now been wrong twice. The original blamed the containment check;
the first correction concluded it was "NOT because the guarded inline check
fails". Both were half right — **both clauses fail, on different collectors,
for unrelated reasons**, and each collector's 100% failure rate is what made the
two look like one.

Found 2026-08-17 on `dev` @`a276dfe09` while profiling the bc-java PQC
throughput page, since retired to
fixed-suite-bugs/bc-java/bug-bcjava-pqc-lms-hsstests-interpreter-throughput-cliff-20260816-FIXED.md.
Split out because it is not a bc-java defect: it is a whole-VM JIT
throughput cost on **every** collector, and any field-dense compiled workload
pays it.

Not a correctness defect. The helper is the *correct* path; it is just the
expensive one, and it is being taken 100% of the time by a fast path built
specifically to avoid it.

## The measurement

`perf record` over a field-dense compiled kernel (BouncyCastle's
`SHA256Digest`, 200 000 iterations of a 64-byte digest, JIT fully engaged,
`hot_but_stuck_in_interpreter=0`, 14 of 15 methods at C2):

| symbol | ZGC (default) | Generational |
|---|---|---|
| `VmHeap::is_object_address` | 11.5% | — |
| `ZObjectStarts::contains` | 10.4% | — |
| `GenerationalHeap::is_object_address` | — | 22.1% |
| `jit::helpers::jit_getfield` | 9.0% | 10.1% |
| `field_layout::object_body_size` | — | 5.2% |
| **getfield helper chain, total** | **~31%** | **~38%** |

Split by DSO: 50.8% in the VM binary vs 47.0% in JIT-compiled code (ZGC);
57.9% / 40.8% (Generational). The real `HSSTests` workload profiles the same
way (55.7% / 43.5%, chain at 31.8%), so this is not an artefact of the
microbench.

`perf`'s caller attribution puts every one of those calls under
`cratonvm_jit::osr_trampoline` — they come from compiled code, not from the
interpreter.

## What `jit_getfield` costs

Per field read: a boundary note (`note_jit_boundary`), `plausible_heap_pointer`,
**a full `is_object_address` heap-membership validation**, a slot bounds check,
a compact-layout lookup that re-reads the class layout registry, and only then
the load. On ZGC the membership test walks `ZObjectStarts::contains`; on
Generational it is `object_body_size` + region walk.

## The fast path exists and is emitted

`CRATONVM_DBG_COMPACT_INLINE=1` reports **35 compact-inline `getfield` sites**
emitted for this kernel. So neither `narrow_oops_block_inline_fields()` nor the
`inline_getfield_enabled()` / `guarded_inline_getfield_enabled()` gates are
turning the emission off. The code is there.

What fails is the **runtime** guard —
`Compiler::emit_guarded_getfield_receiver_check` (`jit/src/x64/objects.rs`),
which null-checks, alignment-checks, and then tests the receiver against the
three `[base, end)` pairs in the process-global `JIT_REGION_BOUNDS` table
(`gc/src/gen_heap.rs`). Receivers that fail branch to `CALL jit_getfield`.

On ZGC the table is **never published**. `gen_heap.rs`'s own doc says so:

> All-zero entries match nothing, so before the first publish — or for heap
> backends that don't publish (G1/ZGC) — every guarded site simply falls through
> to the helper.

`store_region_bounds_locked` is the only writer and it lives on
`GenerationalHeap`. ZGC has been the **default collector since 2026-08-10**, so
since that date the default configuration has taken the helper on every
compiled field read.

## CORRECTION (2026-08-17, same day): the title is wrong, and so were both hypotheses

Instrumenting it — which is what this page asked for — refuted its own story
three times over. Recorded in full because each refutation cost a build, and the
next person should not re-run them.

**1. "Sites cannot resolve a compact layout."** Refuted. A compile-time census
over every field site (`CRATONVM_DBG_COMPACT_INLINE`, extended to report
`MISS`) reports **50 inline sites emitted and ZERO misses**. Every field site
resolves its compact offset.

**2. "ZGC never publishes `JIT_REGION_BOUNDS`, so the check always fails."**
True as a *fact*, false as *the cause*. A runtime helper-call counter across
collectors:

| collector | `jit_getfield` calls |
|---|---|
| ZGC (default) | 49 632 791 |
| Generational | 48 973 210 |
| G1 | 49 632 591 |

Generational publishes real bounds and pays exactly the same price. A cause that
is absent on one collector cannot explain a number identical on all three.

**3. "The guard rejects live receivers."** Refuted directly. Dumping the
receiver beside the six live bounds words on Generational:

```
receiver=0x20042400db8 aligned8=true
bounds=[0x20042400000, 0x20052400000, 0x20054400000, 0x20064400000, 0x2000e000000, 0x2002e000000]
```

`0x20042400db8` is inside `[0x20042400000, 0x20052400000)` — it satisfies null,
alignment *and* containment, so it should have taken the inline branch.

### What is actually happening

Attributing every emitted `CALL jit_getfield` to its emission arm, then turning
the optimizing tier off:

| arm | CALL sites emitted |
|---|---|
| single-pass compact-inline slow path | 50 |
| **IR (optimizing) tier fallback** | **4** |

| configuration | helper calls |
|---|---|
| default (both tiers) | 48 972 303 |
| C2/IR threshold raised out of reach | **5 706 715** |

**~88% of the calls come from four sites in the optimizing tier**, whose
`ir_lower::emit_inline_getfield` returns `false` and emits an **unguarded**
`CALL jit_getfield`. That path never reads `JIT_REGION_BOUNDS` at all — which is
why the count is collector-independent, and why an in-bounds receiver still
reached the helper.

So the guarded inline check is **not** "always failing". It is largely not being
reached: the hot method is compiled by the tier that declines to inline in the
first place. The single-pass slow path is real but is the minority (~12%).

### The refusal reason, and a fix that closed it without moving the number

Counting the seven early-outs named exactly one:

```
IR-tier inline-getfield refusals: width-not-int-category=4
```

`emit_inline_getfield` refused every field that was neither a reference nor
int-category — i.e. **every `long`, `double` and `float`**. A `long` on a hot
path is not exotic (BouncyCastle's `GeneralDigest.byteCount` is one), so this
was fixed: `J`/`D` as 8-byte compact loads, `F` as a 4-byte one (the helper
ZERO-extends `f.to_bits()`, so `MOVSXD` would corrupt every float with bit 31
set), each admitted only when the IR node's own type agrees with the descriptor.

**The fix works and changes nothing.**

| | helper calls | arms emitting a CALL | refusals |
|---|---|---|---|
| before | 48 974 370 | `sp-compact-inline-slowpath=50` `ir-helper-fallback=4` | `width-not-int-category=4` |
| after | 48 973 916 | `sp-compact-inline-slowpath=50` | *(none)* |

The four unguarded fallback sites are gone and the refusal count is zero — and
the helper count is identical. **Keep the fix** (it closes a real gap and
removes four unguarded `CALL`s) but do **not** bank it as a throughput win: it
is worth zero on this workload.

### MY OWN MIS-ATTRIBUTION, recorded so it is not repeated

The "~88% comes from the IR tier" claim above was **wrong**, and it was wrong in
the same way the page's original hypotheses were: inferred from a configuration
A/B instead of measured. Raising `CRATONVM_TIER_C2_THRESHOLD` moved helper calls
48.9 M → 5.7 M, and I read that as "the IR arm was executing 43 M times". It is
not — raising the threshold changes *which code is compiled at all*, not just
which arm emits. The fix above proves it: removing every IR fallback site left
the count untouched.

**Emission counts are not execution counts.** The census answers "which arms
emitted a CALL", which is not the question. Nothing here has yet measured which
CALL *executes*.

### The instrument this actually needs, and the site the census missed

There is a **sixth** helper call site that was never tagged:
`ir_lower.rs`'s `emit_inline_getfield` calls the helper from **its own slow
path**, not only from the fallback. So a site can be counted as "inlined" and
still take the helper on every execution.

The open contradiction, stated plainly so the next attempt starts from it:

* on Generational the rejected receiver `0x20042400db8` is **inside** published
  region 0, so it passes null, alignment and containment;
* the single-pass compact arm routes a *non-compact* receiver to an inline
  legacy read, not to the helper;
* yet ~49 M calls arrive, on every collector.

At least one of those three statements is false, and no configuration A/B can
say which. **The next step is per-call-site EXECUTION counting** — a distinct
thin helper wrapper per emission arm (including `ir_lower.rs:2444`), each
bumping its own counter before tail-calling `jit_getfield`. That is the only
instrument that attributes an execution rather than an emission, and this page
has now cost four hypotheses for want of it.

## The part that is not yet explained

Generational *does* publish the table, and its profile still shows the helper
chain at ~38%. So "ZGC does not publish" is necessary but not sufficient — some
second condition fails the guard on Generational too.

The kill-switch A/B says the same thing from the other side. Forcing helper-only
with `CRATONVM_JIT_GETFIELD_HELPER=1` did **not** slow anything down; on
Generational it was consistently *faster* than the default:

| arm | round 1 | round 2 |
|---|---|---|
| default (ZGC) | 69 102 | 57 072 |
| ZGC, helper forced | 62 515 | 68 163 |
| Generational | 97 444 | 78 975 |
| Generational, helper forced | **61 029** | — |

(ns/op, contended host — treat as orders of magnitude, not a benchmark. The
ordering is what matters.)

A fast path that costs measurably *more* than the helper it is meant to avoid is
a fast path that never takes its fast branch. **The inline guard is pure
overhead today.**

## Step 1 is DONE — the counter, and the price (2026-08-18)

Both came from `probes/AccessorDispatchProbe.java`, built for
fixed-suite-bugs/bug-commonsmath-bobyqa-numeric-kernel-was-an-osr-admission-failure-FIXED-20260819.md,
whose whole 80x gap turned out to be this page.

**The counter.** `jit_getfield` increments `GETFIELD_HELPER_CALLS`, printed as
`[GETFIELD_CENSUS] helper_calls=…` on the existing `CRATONVM_DBG=mic-prof` dump.
Reaching that function at all means the guard fell through, so the count IS the
miss count; the probe supplies an exact denominator, because four of its arms do
`rounds × per` field reads and nothing else:

| rounds | field reads | `helper_calls` | miss rate |
|---:|---:|---:|---:|
| 3 | 24 000 000 | 23 993 196 | 99.972% |
| 5 | 40 000 000 | 39 993 236 | 99.983% |
| 8 | 64 000 000 | 63 993 131 | 99.989% |

The shortfall is **constant at ~6 800, not proportional**, which says more than
the percentage: the inline branch works for a few thousand reads at startup and
then never again. In steady state the fast path is taken **zero** times. The
page's central claim is now a number.

**The price.** One `getfield` from compiled code is **9.7 ns** (Windows) /
**13.2 ns** (Azure Linux), isolated by subtracting an arm that passes the array
as an argument from one that reads it out of `this`. For scale, entering and
leaving the whole compiled callee is 3.4 / 6.7 ns, and HotSpot reads both taxes
at ≤ 0.06. `perf` agrees: `jit_getfield` 9.89% + `is_object_address` 6.41% +
`ZObjectStarts::contains` 4.40% = **20.7% of the run, two-thirds of all
VM-binary time**.

## What to do next — with two of the three old candidates now refused

The plan below used to have three items. Two of them have been measured and do
not work; keeping them on the list would cost the next person the same two days.

1. ~~**Decide whether ZGC should publish its arena bounds.**~~ **Refused by
   measurement.** Generational already publishes `JIT_REGION_BOUNDS` and is
   *slower* on the probe, not faster: `callTax` 15.2/16.4/15.8 against ZGC's
   13.3/12.8/13.1, three reps each. Publishing bounds cannot be the fix while
   the collector that publishes them is behind the one that does not.
2. ~~**A per-field-KIND gate** (primitive fields load inline, reference fields
   keep the barrier).~~ **Refused by measurement.** The probe's `primFieldGet`
   arm reads a primitive `int` field and costs **8.7 ns — the same** as the
   `double[]` reference read's 9.7. The reference/primitive distinction is real
   for *soundness* (a compact reference slot holds
   `Z_COLORED_TAG | colour | offset`, not a pointer — see
   `zgc_read_barrier_blocks_inline_fields`, stage (a) of
   `feature-designs/zgc-jit-load-barrier.md`), but it is not the discriminator
   for *speed*, because primitive reads miss the guard just as completely.
3. **The discriminator is downstream of both the collector and the region
   table.** Same probe, same 40 000 000-read denominator, the counter under each
   candidate:

   | arm | `helper_calls` | miss rate |
   |---|---:|---:|
   | default (ZGC) | 39 992 012 | 99.98% |
   | `CRATONVM_JIT_INLINE_GETFIELD=1` | 39 991 991 | 99.98% |
   | `--XX:UseGc Generational` | 39 993 021 | 99.98% |

   `INLINE_GETFIELD=1` emits the raw form — the six region compares are gone
   from the disassembly, leaving only a null check and `test byte [rax+0Fh],4`
   (`GC_FLAG_COMPACT`, `types/src/heap_types.rs`) — and the miss rate does not
   move. The receiver cannot be the null case: the loop completes 10 000 000
   successful reads per arm. So the fall-through survives removing the region
   test *and* switching to the collector that publishes it, which leaves the
   compact-layout tag test and the site's own admission as the two remaining
   candidates.

   Note that ZGC *does* set the flag — `zgc.rs`'s allocator calls
   `header.add_gc_flags(GC_FLAG_COMPACT)` and its comment says "set at
   allocation". So either these objects are not getting it, or the emitted site
   for these fields is not a guarded-inline site at all and the disassembled
   inline branch is unreachable by construction. Distinguishing those two is one
   read of a `Vec` instance's header, and the instrument for it is now in place.

## ANSWERED 2026-08-18 — it is TWO defects, and the first one is fixed

The section above closes with exactly the right question — *"either these
objects are not getting it, or the emitted site is not a guarded-inline site at
all … distinguishing those two is one read of a `Vec` instance's header"*. This
is that read, plus the instrument that makes it a census rather than a
sample.

### The instrument

`jit_getfield` now classifies **every** receiver into the guard's own clauses,
in the order both backends emit them, behind
`CRATONVM_DBG_GETFIELD_RECEIVERS=1` and printed with the rest of
`CRATONVM_DBG_JIT_METHOD_STATS`:

| bucket | meaning |
|---|---|
| `implausible-or-null` | fails the null / 8-alignment clause |
| `outside-published-bounds` | plausible, outside every `JIT_REGION_BOUNDS` region |
| `legacy-layout-receiver` | in bounds, `GC_FLAG_COMPACT` **clear** |
| `compact-eligible` | passed all three — a receiver the inline path should have read |

The four are on the one path every fall-through crosses, so they sum to
`GETFIELD_HELPER_CALLS` and no emission site can hide from them.

### The answer

SHA256Digest × 200 000, one binary, three collectors:

| collector | helper calls | failing clause |
|---|---:|---|
| ZGC (default) | 68 722 291 | **100% `outside-published-bounds`** |
| Generational | 68 721 857 | **100% `legacy-layout-receiver`** |
| G1 | 68 722 213 | **100% `outside-published-bounds`** |

**Two independent defects that happen to produce the same number.** And the
reason every collector A/B on this page read as "identical, therefore a common
cause" is now plain, and is the methodological lesson of the whole page:

> When the inline path fails **100%** of the time on every collector, the helper
> count is simply how many field reads the PROGRAM does. It is
> collector-independent *by construction*, whatever the reason. Identical counts
> across collectors were never evidence of a shared cause — and four hypotheses
> were built on reading them as if they were.

### Defect 2 (Generational): the receivers really are legacy, and almost everything is

`org/bouncycastle/crypto/digests/SHA256Digest`, 100% of the calls, with
`declared_fields=14 num_slots=14 registered_layout_fields=Some(14)
array_length=0 gc_flags=0x00`. A matching compact layout exists; the object was
nevertheless **allocated** legacy — `array_length=0` and an empty flags nibble
are exactly what `plan_object_alloc` writes on its legacy branch — and it never
appeared in the `[compact-legacy]` allocation census.

It never appeared because it never went through `plan_object_alloc`.
`init_object_header` (`vm/src/runtime/interpreter/gc_and_alloc.rs`), reached
from the TLAB fast path that its own comment calls *"the steady-state path for
~99% of allocations once the adaptive sizer has settled"* — for both the
interpreter and `jit_new_object` — writes `array_length = 0` and no
`GC_FLAG_COMPACT`, unconditionally. Its own comment says so: *"the interpreter
TLAB fast path bypasses gen_heap, so record the **legacy-layout** object header
it writes here."*

So **~99% of objects in the VM are legacy-layout**, whatever their class's
registered layout says, and the `[compact-legacy]` census cannot see it because
that census only reports the paths that consult the planner.

The IR tier's inline `getfield` reads *only* compact receivers — "the one
simplification against the single-pass version", per its own doc comment. An arm
that inlines only compact receivers, in a VM where almost nothing is compact,
inlines almost nothing.

**Fixed** by giving the IR arm the legacy branch the single-pass arm has always
had (the uniform 16-byte `Value` cell at `HEADER_SIZE + field_index*SLOT_SIZE`),
transcribed from that arm rather than re-derived:

| Generational | helper calls | ns/op |
|---|---:|---:|
| before | 68 722 450 | 25 638 |
| after | **0** | **8 347** |

**3.07x**, checksum unchanged, and the engagement counter reads a literal zero —
which on this page is the only form of "fixed" worth accepting.

### Defect 1 (ZGC and G1): the containment clause, and why candidate 1 stays refused

The page already refused "publish ZGC's arena bounds" on a throughput argument.
There is a second and much harder reason, and it should be recorded so nobody
re-opens it: **the empty table is load-bearing GC correctness.**
`audits/g1-audit.md` §8.1 (G1-2, 2026-07-31) made `region_bounds_are_live` the
gate for every inline reference-**store** fast path, precisely so that under
G1/ZGC none of them is reachable and a JNI-pinned, CSet-excluded region cannot
lose its remembered-set edge. Filling the table to speed up *loads* would
silently re-enable those *stores*. Under ZGC it would additionally make inline
reference loads reachable, and a compact reference slot there holds
`Z_COLORED_TAG | colour | offset` — the un-barriered colored word
`heap.rs::read_prim_element` panics on by design.

What was done instead is the shortcut the single-pass backend already had and
the IR tier did not: `emit_trusted_oop_receiver_check`, a bare null test for a
receiver the type system already proves is an oop, **restricted to primitive
fields** so no inline reference load is added anywhere.

| collector | helper calls | ns/op |
|---|---:|---:|
| ZGC | 68 722 592 → 56 931 309 | 24 019 → 22 054 |
| G1 | 68 722 345 → 56 932 041 | 25 586 → 22 722 |

Partial, and named as such.

## What is still open

1. ~~**The remaining ZGC/G1 misses are the SINGLE-PASS arm's containment
   check**, which is not getting its trusted-oop shortcut because it requires
   `stack_oop_marks_exact`.~~ **Answered 2026-08-18, and the premise was wrong.**
   `stack_oop_marks_exact` is not false at these sites. A per-clause diagnostic
   on `receiver_is_trusted_oop`'s three conjuncts (under
   `CRATONVM_DBG_COMPACT_INLINE`, emission-time only) prints **zero** refusals
   across `probes/AccessorDispatchProbe.java` — the single-pass arm takes its
   shortcut everywhere.

   The residual misses are the **IR/C2** arm, and they are not a bug: its
   trusted-oop shortcut is primitives-only *by design*, and the blocking site
   prints `getfield pc=1 off=0 ref=true`. Measured on Azure Linux, quiet host,
   after `a6f0ecf75` + `8787edbbe`:

   | field kind | ZGC | Generational |
   |---|---:|---:|
   | primitive `int` | **1.51 ns** | **1.51 ns** |
   | reference `double[]` | **25.2 ns** | **0.95 ns** |

   Primitive reads are fixed on both collectors and call the helper zero times.
   A reference read is **26x** more expensive on ZGC than on Generational,
   which is exactly the colored-pointer constraint this page's item 3 already
   names — so what is left is not a guard to repair but the load barrier to
   build. That makes item 2 below (a read-side bounds table) the real successor
   to this page, not a fourth sub-problem here.

   **The third collector, and the reason item 2 survives this.** The same
   question asked as an EXECUTION census — `jit_getfield` bucketing every
   `outside-published-bounds` call by whether the field read is a reference —
   agrees on all of the above and adds G1, which the timings above do not
   cover. `SHA256Digest` x200 000, one binary, counts only:

   | collector | helper calls | primitive | reference |
   |---|---:|---:|---:|
   | ZGC (default) | 56 932 090 | **0** | 56 932 090 |
   | G1 | 56 930 831 | **0** | 56 930 831 |
   | Generational | 0 | 0 | 0 |

   G1's residual is the same size and the same shape as ZGC's — and G1 has **no
   colored pointers**. A reference field there is a plain pointer, so those
   56.9M are pure containment failures with no soundness obstacle behind them
   at all. ZGC must wait for the load barrier; **G1 could be fixed today**, and
   that makes item 2 a G1 fix rather than the general containment fix it was
   written up as.
2. **DONE 2026-08-18 — the READ-SIDE bounds table.** Landed as
   `JIT_READ_BOUNDS` (`gc/src/gen_heap.rs`) + `read_bounds_addr` (helper ABI
   v6). What follows is the design as written before the change, kept because
   the reasoning is what made it safe; the two places reality differed from it
   are marked **[REVISED]**.

   *What it bought — the counter.* SHA256Digest x200 000, two binaries built
   from `dev`@`1f41cb193` +/- this change:

   | collector | baseline | patched |
   |---|---:|---:|
   | Generational | 0 | 0 |
   | **G1** | **56 930 918**, 100% `outside-published-bounds` | **0** |
   | ZGC | 56 930 768 | 56 930 752 (unchanged, by design) |

   Reproduced exactly on three separate runs against two different base trees.
   This is the load-bearing result: it is a COUNT, so host load cannot move it.

   *What it bought — the clock, quoted carefully.* The host was shared and at
   load ~6/8 throughout, so this is six pinned (`taskset -c 6,7`) base/patched
   pairs run back-to-back, reported as a sign test and a range rather than a
   point estimate:

   | collector | pairs favouring patched | ratio range | median |
   |---|---|---|---|
   | **G1** (changed) | **6 / 6** | 1.33x - 2.14x | **1.39x** |
   | Generational (unchanged) | 3 / 6 | 0.95x - 1.19x | 1.01x |
   | ZGC (unchanged) | 4 / 6 | 0.92x - 1.81x | 1.31x |

   The two control rows are the point. Both emit byte-identical code before and
   after this change, so their spread IS the noise floor, and ZGC's is nearly as
   wide as G1's effect — its concurrent threads make it the most contention-
   sensitive of the three on two pinned cores. Generational, which is neither
   concurrent nor changed, gives the honest floor at 0.95-1.19x, and G1's
   1.33-2.14x sits outside it in every pair. A cleaner number needs a quiet
   host; the counter above does not.

   *The first timing table on this page was wrong, and the reason is worth
   more than the number was.* It read G1 4111 ms -> 1747 ms (2.35x), measured
   against a base tree that predated `c88ee725a` — "jit_getfield read an
   environment flag on every call", a 3.4x cost INSIDE the very helper this
   change is about avoiding. So the baseline arm was paying a tax that `dev`
   had already removed, and the change looked roughly twice as good as it is.
   Nothing about the measurement was sloppy; the base commit was simply four
   days stale on a file under active repair by someone else. **Re-base the
   baseline before quoting a speedup, especially when the function under test
   is somewhere other people are also working.** The counter was immune,
   because a count of calls does not care what each call costs.

   *The instrument mattered, again.* The first A/B used
   `CRATONVM_JIT_GETFIELD_HELPER=1` as the "before" — one binary, no rebuild,
   and the switch this page's own transferable section praises. It reported a
   1.42x improvement **on ZGC**, a collector this change does not touch. The
   switch also disables the trusted-oop shortcut, which already worked there.
   A kill switch answers "is this whole path worth anything", which is the
   question this page asked in August; it cannot answer "is THIS EDIT worth
   anything". Two binaries was the only way. The same reading error this page
   is about — a number that agrees with the hypothesis for an unrelated reason
   — nearly closed it a second time.

2. **The proper fix for containment under a non-publishing collector is a
   separate READ-SIDE bounds table.** This is a design, not a bug fix, and
   deserves its own page — but the shape is settled enough to write down, so
   the next person does not have to re-derive why the obvious thing is wrong.

   *Why a second table and not the existing one.* `JIT_REGION_BOUNDS` is doing
   two jobs at once. Its documented job is "is this address mapped, so a raw
   load cannot fault" — a READ-side question. Its actual load-bearing job,
   since G1-2, is "may an inline reference STORE skip the collector's write
   barrier" — and G1/ZGC answer that by leaving the table empty. One table,
   two questions, opposite answers. Filling it to fix loads breaks stores.

   *The shape.* A `JIT_READ_BOUNDS` sibling, same six-word layout so the
   emitted containment sequence is byte-identical and only the baked address
   changes, published by every collector with its mapped envelope:

   | collector | source | property relied on |
   |---|---|---|
   | Generational | the three arenas, as today | already refreshed at GC start/end |
   | ZGC | `ZgcRealHeap::conservative_addr_span()` → `[arena_base, arena_end)` | "allocated once in `with_capacity` and never grown", read without the arena lock |
   | G1 | the reserved heap range | **[REVISED]** it fits, and easily: G1's N regions are carved from ONE contiguous `Box` arena, so `[arena_base, arena_end)` in slot 0 covers every region and slots 1-2 stay zero. Published in `G1Collector::new`, cleared in a `Drop` impl G1 did not previously have |

   `region_bounds_are_live` keeps reading the OLD table and keeps gating the
   store paths; only `emit_guarded_getfield_receiver_check` and
   `ir_lower`'s copy of it move to the new one.

   *The ZGC obligation that comes with it.* Publishing read bounds makes the
   inline path reachable for REFERENCE fields under ZGC, where a compact
   reference slot may hold `Z_COLORED_TAG | colour | offset`. That is the
   use-after-free `heap.rs::read_prim_element` panics on by design. So the
   read-side table must land together with a per-field-kind gate that keeps
   reference loads on the helper under ZGC — the same restriction the
   trusted-oop shortcut already carries, applied to the containment path too.
   `zgc_codegen_honours_read_barrier()`'s doc comment states the obligation
   from the other side and must be revisited in the same change: it returns a
   constant `true` and says so **only** while no inline reference emission
   happens under an armed barrier.

   **[REVISED]** ZGC ended up publishing NOTHING into the read table, so the
   per-field-kind gate was never needed. Not publishing is a strictly stronger
   discharge of the same obligation — it keeps ZGC's PRIMITIVE reads on the
   helper too — and it costs G1 nothing, because G1's split is 0% primitive.
   `zgc_codegen_honours_read_barrier` was re-examined rather than assumed and
   stays `true`, now for two independent reasons (ZGC publishes nothing; and
   `narrow_oops_block_inline_fields` suppresses the EMISSION outright while a
   barrier is armed). Both are written down at the function, along with which
   one survives someone later deciding ZGC should publish after all.

   *What it is worth, now that item 1 is measured.* **G1 only, and there it is
   worth all 56.9M.** The split came back 100% reference / 0% primitive, so:

   * on **ZGC** the table buys nothing on its own — the reads it would admit
     are exactly the ones the colored-pointer representation forbids inlining,
     so the real gate is the ZGC JIT load barrier and this table must not land
     ahead of it;
   * on **G1** there is no colored-pointer obstacle at all, and the entire
     residual is containment failures on plain pointers. A read-side table is
     the whole fix.

   That inverts the original priority: this was written up as the general
   containment fix and it is really a G1 fix. Scope it that way — Generational
   already publishes, ZGC must wait for the barrier, and only G1 is left
   paying for a table it could fill today.
3. **`init_object_header` should honour the compact layout.** Defect 2 was fixed
   on the *reader* side, which is right and enough for `getfield` — but the
   underlying fact remains that ~99% of allocations ignore a registered compact
   layout, and the memory-footprint and cache consequences of that are not
   measured anywhere. This is an ALLOCATOR change on the hottest path in the VM
   and is deliberately out of scope for a getfield page.
4. **The `[compact-legacy]` census has a blind spot** and now says so: it reports
   only paths that consult `plan_object_alloc`. A companion census was added for
   the JIT's inline TLAB allocator; the interpreter/`jit_new_object` TLAB path
   still has none, and that is the path that matters most.

## CORRECTION 2026-08-18: every ZGC number on this page was inflated 3.4x by a diagnostic added to this page

`1794c8e81` ("dump the receiver beside the live bounds table on helper entry")
gated its `#[cold]`, 8-line-bounded dump on a bare

```rust
if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE").is_some() {
```

**inline in `jit_getfield`** — an uncached, string-keyed flag lookup on the path
this VM takes tens of millions of times a second. The line directly above it
caches its flag in a `OnceLock`; this one did not. The dump was never the cost.
The gate was.

Two sessions found this independently within an hour, by different routes, and
both are worth keeping: a per-key flag-read census on `BigDecimalBench` counted
**4 560 891 of 4 600 000 flag reads (99.1%) for this one name**, ~91 per
benchmark iteration; and the bisect below priced it. The census says how often,
the bisect says how much.

Bisected on an Azure host with `probes/AccessorDispatchProbe.java`, all arms
interleaved in the same rounds so a shared host cannot bias one against another,
`receiverFieldTax` (one reference-field read):

| binary | ns |
|---|---|
| `bc01a0066` — before the diagnostics | 13.30 / 13.85 / 13.05 |
| `9c74737a6` — after the four `diag` commits | 52.75 / 51.97 / 50.20 |
| `dev` @ `36433bf5d` | 49.12 / 46.61 / 49.23 |
| **the same, with the gate cached** | **14.11 / 13.46 / 14.49** |

**3.5x, recovered by one line.** Re-verified after merging current `dev`, same
interleaving: `bc01a0066` 13.02 / 13.48 / 14.09, `dev` 50.57 / 50.08 / 45.80,
fixed 14.47 / 13.39 / 13.00 — back to baseline.

The neighbouring `GETFIELD_HELPER_CALLS` atomic is a second, much smaller cost:
an ablation build put it at ~2-3 ns of a then-9 ns read on a quiet host, and
below the noise floor under load. It is now gated behind the flags that
actually read it, and `jit_getfield_helper_calls()` returns `Option<u64>` so a
gated counter cannot be printed as a confident `0` — which would look exactly
like a fast path that never fell through.

### What this invalidates

Every ZGC/G1 timing on this page was taken between `1794c8e81` and this fix, so
each carries ~33 ns of flag lookup on the helper path. **Only the paths that
CALL the helper are affected** — a primitive field inlines and never enters it —
so the corrections are one-sided and the reference-vs-primitive comparisons here
overstate the gap:

| | as published | re-measured with the gate cached |
|---|---:|---:|
| ZGC, reference field | 25.2 ns | **12.5 – 15.3** |
| Generational, reference field | 0.95 ns | 2.1 – 3.4 |
| ZGC, primitive field | 1.51 ns | 1.8 – 1.9 |
| **ZGC-vs-Generational reference gap** | **26x** | **~5x** |

The direction of every conclusion survives — reference reads under ZGC really do
take the helper, really are the residual, and Generational really does inline
them. The *magnitude* does not: it is 5x, not 26x, and the case for the
read-side bounds table has to be argued at 5x.

Counts are unaffected: `helper_calls` is a count, not a timing, and every
engagement figure on this page still stands exactly as printed.

### The methodological point, which this page is the right home for

This page's own thesis is that an instrument can be the thing you end up
measuring. It then measured itself for a day. The tell was available the whole
time and was read as noise: the "before" number kept coming out at 13 while
every later arm sat at 25-50, and that was attributed to host load — on a box
that genuinely was loaded, which is what made the excuse plausible. What settled
it was interleaving all arms inside one round so load could not favour one, and
the discipline that catches this in general is: **when a diagnostic lands on a
hot path, price it in the same run that uses it.**

## The transferable part

**A fast path that is emitted is not a fast path that runs**, and it took a
counter to say so: the miss rate is 99.98%. Three separate
signals agreed the inline `getfield` was live — the gates are default-on, 35
sites were emitted, and the codegen arm is exercised by unit tests — and all
three are about *emission*. Only the profile and the kill-switch A/B asked
whether the inline *branch* was ever taken, and the answer was no.

**The kill switch answered in one run what the gate reading could not.** Reading
`narrow_oops_block_inline_fields`, `compact_ref_fields_enabled`,
`guarded_inline_getfield_enabled` and `region_bounds_addr != 0` produced a
plausible story that was wrong. `CRATONVM_JIT_GETFIELD_HELPER=1` settled it.

**And then the same kill switch was the wrong instrument for the fix.** It
scopes to "the whole guarded path", which is the right scope for *is this worth
building* and the wrong one for *did my edit do anything* — it also disables the
trusted-oop shortcut, so it credited this change with a 1.42x speedup on ZGC,
which it does not touch at all. An instrument is only as good as the question,
and the two questions were one clause apart. Two binaries from the same tree
cost ten minutes and had no such gap.

**A 100% failure rate makes its own count uninformative.** This is the one that
cost the most. Every collector A/B here returned the same number, and that was
read five separate times as "the cause is collector-independent". It is not
evidence of anything: when the fast path never fires, the helper count is the
program's field-read count, and any two collectors will agree on it for
completely different reasons. The counter that had to be built was not
"how many" but **"which clause"** — a census over the guard's own branches,
whose buckets sum to the total. It cost one build and answered a question four
hypotheses had failed at.

**A census is blind to the paths that do not consult it.** `[compact-legacy]`
faithfully reported every legacy allocation that went through
`plan_object_alloc`, and SHA256Digest was absent from it while being 100% of the
legacy receivers — because the TLAB fast path, ~99% of all allocations, does not
call the planner. An absence in a census is only evidence if you know the census
covers the path.

## ZGC residual, part 1: stop asking `is_object_address` (2026-08-18, ~1.05x)

The walk is **validation**, not correctness — it defends against a stale
receiver from a miscompiled frame. Where the IR types the base node `Ref` we
already have that proof; it is the same proof the PRIMITIVE trusted-oop arm
relies on, and that arm goes further and does a raw inline load off the very
same receiver. So the reference slow path now tells the helper to skip it.

**This is not the thing the page refused to do.** The colouring hazard is about
the loaded VALUE; containment validates the RECEIVER. Nothing here inlines a
coloured load, publishes `JIT_REGION_BOUNDS`, or touches the inline path.

Carried as a bit in `field_index` (`GETFIELD_RECEIVER_PROVEN_OOP`), not a new
helper slot: `helpers_abi.rs` pins the table's field count, byte size and golden
offsets with const assertions plus an ABI version, all so the offsets the JIT
bakes cannot move. A first attempt added a slot and the guards refused it,
correctly.

| check | result |
|---|---|
| engagement | **34 470 791 of 34 470 791** helper calls take it — 100% |
| checksum, all three collectors | **MATCH** |
| ZGC wall, 3 interleaved rounds | 5708→5420, 5646→5448, 5653→5378 — **~1.05x** |
| Generational / G1 | **0 helper calls** — unaffected, and measured so rather than argued |

### Why only 5% when the walk profiled at 20.6%

Because `getfield` was only about half of it. After the change:

| symbol | before | after |
|---|---|---|
| `ZObjectStarts::contains` | 11.08% | **6.40%** |
| `ZgcRealHeap::is_object_address` | 9.54% | **5.57%** |

Roughly half the membership-walk traffic survives, from **other** helpers
(`jit_putfield_*`, the array helpers) that still validate their receiver the
same way. Extending the same proven-oop argument to them is the obvious next
step and is not done here.

**The Generational A/B rounds also moved (~2-6%) and that was noise**, not an
effect: the engagement counter reads 0 helper calls there, so this change cannot
reach it. Recorded because a 6% shift on a shared host is exactly the size that
invites a false claim — on this page a configuration A/B has already produced
one.

### What now dominates on ZGC

`try_jit_site_cached_native_dispatch` 8.81% + `safe_native_call_impl` 8.33% —
per-call native dispatch, a different page — and `jit_getfield`'s own remaining
body at 16.31%. The membership walk is no longer the single largest item.

## Caller census for the surviving walks — it is NATIVE DISPATCH, not putfield

`perf` could not answer this: DWARF unwinding on the optimized build returns
self-recursive frames, and LBR is unavailable on the virtualised PMU. Grouping
all 132 `is_object_address` call sites by enclosing function pointed at the
native-dispatch path rather than the obvious `getfield` siblings, and counting
confirmed it (SHA256Digest x200 000, ZGC):

| site | membership walks |
|---|---|
| `decode_dispatch_values_into` | **5 495 224** |
| `try_jit_site_cached_native_dispatch` | 439 463 |
| `getfield` | **0** |

Two things follow.

**The getfield fix is complete on its own terms** — zero walks remain from that
arm, where there were ~34 M.

**`putfield` and the array helpers were the wrong suspects.** The surviving
walker is the per-call native argument decode: ~27 walks per loop iteration,
which is exactly the native-call count of this kernel (16 `Pack.bigEndianToInt`
per block x 2 blocks, plus `SHA256Digest.processBlock`). **Every native call
membership-walks each of its reference arguments.**

That means "extend the proven-oop argument" and "per-call native dispatch" —
listed as two separate follow-ups — are **one item**. It also means the SHA-256
intrinsic landed for the bc-java PQC page pays this tax on every invocation, so
the two pages meet here.

The same trust argument should apply: the JIT knows these arguments are oops.
That is the next fix, and it is **not** done here — this section is the census,
not the change.

### Caveat on the census's scope

Only the JIT helpers in `vm/src/jit/helpers.rs` are tagged. `is_object_address`
has 132 call sites across the VM; the GC's and the interpreter's are not
counted, so these numbers are the JIT-side share and not the process total. Do
not subtract them from a profile percentage and expect the remainder to be zero.

## ZGC residual, part 2: validate ONCE per native accessor call (2026-08-20, 1.07x / 1.11x)

The census above identified the surviving walker as per-call native argument
decode. Following that: the `NativeContext` accessors were walking the SAME
`ObjectRef` two and three times inside one call, with no safepoint between —
`array_length` three times (its KINDOF-SENTINEL guard, then `load_and_forward`,
then `kind_of`), `get_field`/`set_field` twice, the six bulk array helpers
twice.

`load_and_forward_checked` now reports whether the ref it hands back is a
validated live base, and the accessors pass that proof to
`class_id_of_validated` / `kind_of_validated` / `element_type_of_validated` /
`load_and_forward_validated` instead of re-deriving it. The checked variant
exists because `load_and_forward` returns its argument **unchanged** on a miss,
so a trusted twin fed from the plain one would dereference a pointer nothing
had checked. Every accessor still validates its own input independently, which
is what the KINDOF-SENTINEL comments require; only the second and third
validation of the same address inside one call goes away.

**Walk count:** 64 248 919 -> 38 879 898, **-39.5%** against a predicted -38.8%.
Checksum MATCHES on ZGC, Generational and G1.

### The wall clock, which the count does not give you

A walk count is not a time on this page — the getfield fix removed 34M walks and
bought ~1.05x — so this landed with a count and no time, and that gap is now
closed. `CRATONVM_GC_NO_VALIDATE_ONCE=1` makes every `*_validated` twin
re-validate, restoring the two-and-three-walk behaviour, so the change is an A/B
inside ONE binary rather than a comparison across two builds.

`probes/Sha256WalkProbe.java` — bc-java's `SHA256Digest` over a 64-byte block,
200 000 iterations after a 20 000-iteration warm-up, which is the same kernel
the census above used and makes ~27 walks per iteration. Interleaved
arm-by-arm, idle host, ms per run:

| collector | validate-once ON | OFF | pairs won by ON |
|---|---|---|---|
| ZGC (default) | 1451 1333 1373 1335 1334 | 1565 1489 1427 1426 1409 | **5 of 5**, mean 1365 vs 1463 = **1.07x** |
| Generational | 1263 1270 1257 1272 1256 | 1445 1390 1398 1376 1381 | **5 of 5**, mean 1264 vs 1398 = **1.11x** |

The probe's checksum was byte-identical in all twenty runs, so the arms are
computing the same thing. Generational moves too, and should: these accessors
are collector-independent, unlike the getfield arms.

**A contended-host read said the opposite.** The first Generational rounds were
taken while a `cargo test` was running on the same host, and they had ON losing
2 of 3 pairs (2478/1664/1284 against 2226/1391/1391 — note the first pair is
nearly double the idle number, which is the tell). Idle, the same binary and the
same command give 5 of 5 the other way. This page has already produced one false
claim from a configuration A/B on a shared host; that is twice now, and the rule
that catches it both times is *look at the absolute numbers, not just the
direction* — a round that is 2x the idle time is not measuring the change.

### The census that found this is retired

The whole-VM `#[track_caller]` per-caller census in `gc/src/vm_heap.rs` is gone
(`IS_OBJECT_ADDRESS_CALLS`, `note_accessor_call`, `note_census_site`,
`is_object_address_callers`, and the `vm-cli` lines that printed them). It
answered its question, and it was not free: an atomic increment plus an
open-addressed probe on every walk, which is what inflated every ZGC number on
this page by 3.4x before that was caught.

What that costs: **the walk count above can no longer be read from a running
VM.** `--diag` still prints "membership walks by JIT site", which is the
hand-tagged JIT-side census and is what the engagement numbers on this page come
from; it does not see `decode_dispatch_values_into` or anything else outside
`helpers.rs`. If the whole-VM count is needed again it is in git history — the
instrument is small, and the reason to rebuild it deliberately rather than leave
it running is the 3.4x.

### What is still open

Unchanged by this: ZGC's **getfield** residual is still 56.9M helper calls, all
`outside-published-bounds`, and still blocked on `feature-designs/zgc-jit-load-barrier.md`.
A compact reference slot on ZGC holds `Z_COLORED_TAG | colour | offset` rather
than a pointer, so inlining its load is the use-after-free that design exists to
prevent. Nothing in this section touches the inline path, publishes
`JIT_REGION_BOUNDS`, or inlines a coloured load — it removes redundant
validation of a RECEIVER, which is the same argument part 1 made.
