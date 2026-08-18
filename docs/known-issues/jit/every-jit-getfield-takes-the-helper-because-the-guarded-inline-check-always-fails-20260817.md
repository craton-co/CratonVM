# Every JIT `getfield` takes the checked helper: the guarded inline path is emitted, and its runtime guard always fails

## Status
**OPEN**, found 2026-08-17 on `dev` @`a276dfe09` while profiling the bc-java PQC
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
