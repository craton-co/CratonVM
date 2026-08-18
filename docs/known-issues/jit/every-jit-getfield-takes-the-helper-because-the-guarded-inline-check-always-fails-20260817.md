# Every JIT `getfield` takes the checked helper — TWO independent guard clauses fail, one per collector family

## Status
**PARTLY FIXED 2026-08-18, and fully diagnosed.** The Generational defect is
closed: 68 722 450 helper calls -> **0**, 25 638 -> 8 347 ns/op (3.07x). On all
three collectors the inline path is now engaged for **every primitive field
read** — 0 primitive misses, measured two independent ways — and the entire
remainder is **reference** reads. On ZGC those are blocked on the JIT load
barrier (a compact reference slot there is a colored word, not a pointer) and
this page is finished. On **G1** they are blocked by nothing: no colored
pointers, plain-pointer reference fields, 56.9M pure containment failures. That
is the one actionable item left, and it narrows item 2 from "the general
containment fix" to "a G1 fix".

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
[`../perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`](../perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md),
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
   | G1 | the reserved heap range | needs checking — G1 has N regions and the table has 3 slots, so this is the one that may not fit |

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
