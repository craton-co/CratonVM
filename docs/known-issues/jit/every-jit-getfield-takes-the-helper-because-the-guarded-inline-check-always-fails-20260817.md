# Every JIT `getfield` takes the checked helper — but NOT because the guarded inline check fails (see the CORRECTION)

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

## What to do next

1. **Instrument before fixing.** Add an engagement counter to the guarded inline
   site — one increment on the inline branch, one on the fall-through — and
   print both. The whole point of this page is that a fast path can be emitted,
   measured, and still never run; a fix priced on anything but that counter is a
   guess.
2. **Then** decide whether ZGC should publish its arena bounds.
   `ZgcRealHeap` holds a single contiguous `Mutex<Arena>`, so
   `[base, base+capacity)` is available and has the same "mapped for the heap's
   lifetime" property the Generational argument relies on.
3. **Reference fields need separate treatment under ZGC.** A compact reference
   slot holds `Z_COLORED_TAG | colour | offset`, not a pointer, so an inline raw
   load of a *reference* field and handing it on is precisely the use-after-free
   that `zgc_read_barrier_blocks_inline_fields` (stage (a) of
   `feature-designs/zgc-jit-load-barrier.md`) exists to prevent. Primitive
   fields need neither a load barrier nor narrow-oop decoding, and `c_is_ref` is
   already known at the emission site — so a per-field-kind gate is available
   and a blanket one is not.

## The transferable part

**A fast path that is emitted is not a fast path that runs.** Three separate
signals agreed the inline `getfield` was live — the gates are default-on, 35
sites were emitted, and the codegen arm is exercised by unit tests — and all
three are about *emission*. Only the profile and the kill-switch A/B asked
whether the inline *branch* was ever taken, and the answer was no.

**The kill switch answered in one run what the gate reading could not.** Reading
`narrow_oops_block_inline_fields`, `compact_ref_fields_enabled`,
`guarded_inline_getfield_enabled` and `region_bounds_addr != 0` produced a
plausible story that was wrong. `CRATONVM_JIT_GETFIELD_HELPER=1` settled it.
