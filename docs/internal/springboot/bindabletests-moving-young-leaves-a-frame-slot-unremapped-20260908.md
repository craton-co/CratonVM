# The stale receiver was never in a frame slot — `URL.openConnection` wrote its PRE-GC address into the carrier's `url` field

| | |
|---|---|
| **Status** | **FIXED**, 2026-09-09. Root-caused; the reported failure no longer occurs at any threshold this page tested. |
| **Fix** | `native-builtins/src/net_phase_e.rs` — pin the receiver across the allocations in `java.net.URL.openConnection`. |
| **Was** | `docs/known-issues/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md` |
| **Residual** | A DIFFERENT defect keeps `CRATONVM_DBG_GC_STRESS <= 262144` failing. It is not this one and it is filed separately — see [Residual](#the-residual-and-why-it-is-not-this-defect). |

## The defect

`java.net.URL.openConnection` takes its receiver as a bare `ObjectRef` and then
allocates repeatedly — `create_string`, `new_object`,
`try_alloc_concurrent_synthetic` — and runs bytecode through `invoke_virtual` /
`invoke_special`. A moving young collection at any of those points relocates the
URL, and `this` keeps naming the address it moved away from.

The write that turns that into a defect is the last one:

```rust
let conn = try_alloc_concurrent_synthetic(ctx, carrier, 16)?;   // can GC
ctx.set_field_by_name(conn, "url", Value::Object(Some(this)));  // dead address
```

It stores a from-space address into a **live** object's field. No frame remap
reaches it — the collection has already run — and the allocator then re-serves
the address to something else. `conn` itself goes stale across
`create_string("GET")`, and `file` / `spec` have the same shape in the `file:`
branch and in `JarURLConnection.getJarFileURL`.

Fixed with the pin idiom the rest of that file already uses: `pin_native_root`
at entry, `read_native_pin` after each allocation point, `unpin_native_roots` on
each exit.

## Why the page looked for it in the wrong place, twice

### The `stack[0]` table was the instrument, not the defect

The page's central evidence was eight reports per run, all `stack[0]`, all in
bootstrap `<clinit>` frames, all with `heap_collection == thread_last_heal`. All
eight were false positives, and so was every other reading from that instrument
on this collector.

`gc_quiescence::note_allocated` is what makes `CRATONVM_DBG_VACATED_FRAMES`
exact: an address the collector moved an object away from stops being evidence
the moment the allocator hands it out again. Its only callers were ZGC's three.
Under `--XX:UseGc Generational` nothing ever removed an entry, and the mutator
bump-allocates straight back into the semispace the previous cycle vacated — so
the ledger answered "vacated" for **every freshly allocated young object**, and
every detector built on it reported the allocation itself as a stale reference.

The producer backtrace was in the same log the whole time, and it is the same
one for all eight: `Instruction::Anewarray` pushing the array `gc_alloc_array`
had returned two statements earlier, with no collection in between. Nothing was
stale; the ledger had forgotten to forget.

With the ledger fixed (a per-object door on every non-TLAB allocation entry
point, and a per-RANGE door at `VmHeap::refill_tlab`, because a TLAB chunk is
bump-allocated from without any further call into the heap), the same run
reports **zero** of those eight and still crashes at the same 1203rd cycle. That
is what separated instrument from defect.

### `thread_last_heal` was being compared against the wrong collection

The ledger accumulates across cycles deliberately, so "vacated" carries no date.
The report compared the thread's `last_heal_collection` against the CURRENT
collection count — always equal at a safepoint — and printed the answer as
though the remap had missed the slot. Each entry now carries the cycle it was
made on, and the report prints `vacated_on` and `remap_reached_this_thread`,
which is the comparison that actually discriminates.

### The `URLClassPath.<init>` lead was 44 collections stale

Already recorded on the page, and it stands: `collections_since=44`,
`CRATONVM_NO_LOCAL_LIVENESS=1` reproduces. That instrument fix was correct and
is kept.

## What actually found it

`CRATONVM_DBG_CCE_BT`'s dispatch-miss dump named the frame chain but not the
slot. Extended to dump the object slots of the innermost three frames with the
class each address resolves to, it answered in one run:

```text
CCE-BT-SLOT[15] local[0] 0x20042575c50 java/util/ServiceLoader$LazyClassPathLookupIterator
CCE-BT-SLOT[15] local[1] 0x20042575fb0 java/net/URL          <-- correct, relocated
CCE-BT-SLOT[15] local[2] 0x20042576020 java/util/LinkedHashSet
CCE-BT-SLOT[15] local[3] 0x7242c14007b0 java/net/JarURLConnection
NSME-RECV addr=0x7242c1400000 ... num_fields=16 [0]=Object [1]=Int(0) [2]=Int(8) [3]=Float(0.75)
```

`parse`'s own `local[1]` holds the correct `java.net.URL` at its relocated
address. The receiver the dispatch used is a different address whose shape is a
`java.util.Hashtable` (`table`, `count`, `threshold`, `loadFactor`). It came
from the `JarURLConnection` in `local[3]` — from its `url` field, written by the
native above — which is why every frame-side verifier in the tree reported clean
on every one of those 1203 cycles. There was never a frame slot to find.

## Verification

`org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64,
`--XX:UseGc Generational`, JIT on, `-Parallel 1`, one class per process.

| `CRATONVM_DBG_GC_STRESS` | before | after |
|---:|---|---|
| *(unset — the default)* | PASS | **PASS 27/27**, three reps |
| 4 194 304 | PASS | **PASS 27/27**, three reps |
| 3 145 728 | PASS | **PASS 27/27** |
| 2 097 152 | PASS | **PASS 27/27** |
| 1 048 576 | PASS | **PASS 27/27** |
| 524 288 | *(untested)* | **PASS 27/27** |
| 262 144 | CRASH at moving cycle 1203 | CRASH at moving cycle **7098** — a different defect, below |
| 131 072 | CRASH at moving cycle 1203 | CRASH, same different defect |
| 65 536 | CRASH at moving cycle 1203 | CRASH, same different defect |

The page's own best property — "1203 in every crashing run, across four
thresholds and three binaries; any change that moves that number has changed the
workload, not the defect" — is what makes the after column readable. The number
moved because the workload now gets six times further, and the failure that
remains has a different signature at a different place.

Before: `NoSuchMethodError java/util/Hashtable.openStream()` from
`ServiceLoader$LazyClassPathLookupIterator.parse @pc=22`.
After: a reclaimed `java.util.function.Supplier` in
`DisplayNameUtils.determineDisplayNameForMethod local[0]`.

## The residual, and why it is not this defect

At `<= 262144` the class still crashes, for a reason this page never described
and that only became reachable once the URL defect was fixed: a Java local
reaches `invokevirtual` holding an interior word of a retired TLAB's tail
filler -- a span that is dead by construction and never held an object base.

**That page is retired too (2026-09-09), and its central reading was wrong.**
The address was not an interior word: a young semispace is reset and re-served
from the same base every cycle, so the same address is a valid object start on
one cycle and inside a filler on the next, and the page was reading the cycle
that reported it rather than the cycle that broke it. The real defect was one of
FOUR of the same family as this page's own -- a bare `ObjectRef` held across an
allocation -- all four now fixed. See
[`bindabletests-stale-objectref-family-across-allocation-20260909.md`](bindabletests-stale-objectref-family-across-allocation-20260909.md)
for the root causes, the eight measurements that exonerate the collector, and
the single residual that remains.

It is the SAME family as this page's defect — a stale reference reaching
bytecode — with the producer not yet named. The difference is where the evidence
sits: here the object exists at a new address and a live field named the old
one; there the address names nothing at all, and it took the whole
`POST-GC RECLAIMED-WHILE-HELD` -> `in_root_set` -> `[forward-refused]` ->
`object_at_nearest_start` chain to establish that the collector was right about
it. None of those instruments existed when this page was written.

## Instruments added or repaired while chasing this

Every one of these is default-off and answers a question that had no answer
before:

* **the vacated ledger's re-issue accounting**, on every collector rather than
  ZGC alone — the fix that made the page's own table readable, with three unit
  tests pinning the per-object door, the range door, and the per-entry date;
* **the class-discriminated moved-history ledger**, likewise fed from
  `update_all_roots` rather than ZGC's `relocate_stw` alone;
* **`CRATONVM_DBG_CCE_BT` slot dump** — the innermost three frames' object slots
  with each address's resolved class, which is what found this defect;
* **the heap-stale walk's dedupe** — it reported at most 40 lines per collection
  and did not dedupe, so one repeated `java/lang/Module field[0]` pair took 2272
  of 2280 lines and no other referrer in the heap was ever reachable by it;
* **`POST-GC RECLAIMED-WHILE-HELD`** — a live frame slot naming an address in the
  semispace the cycle emptied, with no pointer-map entry, which is proof that
  the collection reclaimed an object a frame still held;
* **`in_root_set` / `scan_would_root`** on that report, and **the evacuator's
  three refusal paths**, which is the chain that identified the residual.

## Repro (for the residual only — this page's defect no longer reproduces)

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_GC_STATS=1 \
CRATONVM_GC_RESERVE=0 \
CRATONVM_DBG_ROOT_REMAP_AUDIT=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

`CRATONVM_GC_RESERVE=0` keeps the decommitted granules mapped, which turns the
SIGSEGV into a reportable read — without it the process dies inside the guard
that would have named the defect.
