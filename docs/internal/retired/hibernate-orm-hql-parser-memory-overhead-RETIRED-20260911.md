# `HqlParserMemoryUsageTest`: the counter was wrong in BOTH directions — RETIRED 2026-09-11

**Retires `docs/known-issues/hibernate/hibernate-orm-hql-parser-memory-overhead.md`.**
That record was opened against a 2.5x allocation multiplier, revised itself twice,
and closed its last addendum with one actionable item: *"The actionable target is
the process-wide accumulator introduced by `077d46ed7` … reconciled against the
per-thread counter, which is already correct. Not touched here."*

It has now been touched. The accumulator had **three** defects, not one, and the
per-thread counter it was to be reconciled against was **not** already correct —
it was missing the entire native allocation surface. Every figure that record
ever printed, in every revision including the one that corrected the previous
two, was taken on an instrument that was wrong in both directions at once.

| | |
|---|---|
| **Verdict** | Instrument fixed and verified against retained heap; half the footprint residue it exposed fixed too. The test still **FAILS**, now for a measured reason rather than an artefact. |
| **What is claimed** | Both allocation counters agree with retained heap at **1.00x** on every measurable shape, on all three collectors, with and without a collection inside the window. |
| **What is NOT claimed** | That the test passes. It does not, on any configuration — closest is 263,799 KB against a 262,144 KB budget. The rest of the residue is real object footprint and is handed to a successor record. |
| **Where** | Windows 11, JDK 25 Temurin `25.0.3+9`, branch `claude/hibernate-hql-parser-memory-0cb182` off `dev@5bc72dda3`, classpath `apps/hib-suite-runner/common.args`. |

---

## 1. The defects

All of these sit behind
`com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes()`, which is the
counter `MemoryUsageUtil` prefers and therefore the one the test's 256 MiB
budget is asserted against.

### 1a. The reader added the calling thread's whole history to a global that already held it

`ThreadMXBean::total_allocated_bytes` (`vm/src/vm/vm_exec.rs`) read:

```rust
process_allocated_bytes().saturating_add(self.thread.tlab.thread_allocated_bytes())
```

`process_allocated_bytes()` is fed from every TLAB retire and every
`note_external_allocation`, so it already contains the calling thread's entire
settled total. `thread_allocated_bytes()` is that same total *plus* the live TLAB
span. Adding it back reported, for a single-threaded workload, **exactly 2x**.

The intent was right and stated correctly in the comment beside it ("plus this
thread's live TLAB span"); the code added the total rather than the span. Fixed
by adding `consumed_bytes()` — the live span, the one quantity not yet in the
global.

### 1b. ZGC's heap-internal staging TLAB credited the global a second time

`Tlab` is used at two layers: a Java thread's own bump buffer, and — under ZGC
only — a heap-internal staging buffer (`ZArenaTlab`, `ZTlab`) that the thread's
allocations are carved out of. Both retired into the same process-wide
accumulator, so under ZGC every byte was counted twice there and **3x** in total.

That is exactly the Generational/G1-versus-ZGC split the 2026-09-06 addendum
noticed and could not explain, and its guess about which arm was sound was
backwards for the second time in the record's life.

Fixed with `TlabAccounting` (`gc/src/tlab.rs`): a buffer built by
`Tlab::new_heap_staging` never credits the process total. The default is
`JavaThread`, so a forgotten annotation over-counts loudly rather than
under-counting silently — the latter being the failure mode that passes a byte
budget vacuously.

### 1c. Publication was a blind add, so it could drift in either direction

Both defects above were possible because each publication site added "the bytes I
just added". Replaced with a **high-water mark** (`Tlab::process_published`):
every publication sends `thread_alloc_carry - process_published` and moves the
mark up, and `adopt_allocation_total` carries the mark across a refill alongside
the total. A site that is forgotten is caught up by the next one; a site that
runs twice sends zero the second time. Neither failure mode is reachable by
construction any more.

While in there: `zgc::arena_tlab::tlab_retire_locked` had open-coded
`install_tail_filler(); retire_taking_tail();`. The filler sets `cursor = end` by
design, so the retire that followed read the whole chunk as consumed and charged
the unused tail as allocated — the same defect `Tlab::retire`'s own body
documents having fixed, reintroduced by splitting the two steps apart.
`Tlab::retire_with_filler_taking_tail` now does both in the one order that is
correct.

### 1d. And underneath all of it: the native allocation surface was never counted at all

This is the one the old record could not have found, because its own closing
conclusion — *"read on the counter that is faithful, CratonVM allocates less than
HotSpot on this parse"* — was this defect speaking.

Both counters are fed from exactly two places: the TLAB cursor, and
`note_external_allocation` for what bypassed the TLAB. The interpreter's slow
paths (`alloc_object_shared`, `gc_alloc_array`) and the JIT's copies of them have
always called the second. The **`NativeContext` allocation funnels did not**, on
any of their non-TLAB arms — and every collection, box, string, array and
reflective object in this VM is built through them.

MEASURED, `Integer.valueOf(100000 + i)` × 200,000, against retained heap:

| | retained (truth) | per-thread | process-wide |
|---|---|---|---|
| HotSpot | 16.0 | 16.0 | 16.0 |
| CratonVM Generational, before | 24.0 | **0.0** | **0.0** |
| CratonVM ZGC, before | 24.0 | **0.0** | **0.0** |
| CratonVM G1, before | 24.9 | 24.0 | 24.0 |

G1 was not counting better. G1 is the one backend with neither a native old-gen
batch pool nor a disabled-by-default TLAB refill, so its native allocations
happened to land on the single arm that was already counted. Reading one
collector and calling the others outliers — which this record did twice — cannot
distinguish that from correctness.

Fixed by charging every non-TLAB arm of `alloc_object`, `new_object`,
`new_object_initialized`, `new_array`, `new_ref_array`, `try_new_array`,
`try_new_ref_array` and the four `create_string*` funnels. Sizes are read back
off the allocated object's own header rather than recomputed from the request,
because the two differ: `alloc_object` clamps a caller's slot count up to the
class's real field count, and charging the request over-reported a boxed
`Integer` at 32 bytes where its header says 24. The TLAB arm is deliberately
**not** charged — its bytes are the cursor's advance, which both counters
already read.

---

## 2. What the instrument says now

`probes/AllocCounterFidelity.java`, rewritten. It measured only the *per-thread*
counter before, which is exactly how a record built entirely on the process-wide
one could be opened, argued and half-closed without the instrument ever being
suspected. **Checking one counter is not checking the accounting.** It now
checks both against retained heap, and has a second phase that retains nothing
so a collection runs inside the measured window (the defects here are quietest
when nothing is collected, which is the only regime the old probe had).

| collector | process-wide, before | process-wide, after |
|---|---|---|
| Generational | 2.00x | **1.00x** |
| G1 | 2.00x | **1.00x** |
| ZGC | 3.00x | **1.00x** |

Per-shape, against retained heap (`probes/AllocShapeTruth.java`, new —
`AllocShapeProbe` reports what the counter says with nothing to check it
against): eleven constructor shapes read **1.00** on CratonVM Generational,
including every collection. The two rows above 1.00 are `String(new)` and
`StringBuilder.toString`, where the allocation is transient by construction and
retention cannot see it; HotSpot reads 1.80 and 1.37 on those same two rows.

---

## 3. The number the old record was looking for

`probes/HqlParseAllocProbe.java` (new) reproduces the test's own measurement
window and reads BOTH counters across it, so agreement between two independent
mechanisms is visible rather than assumed.

| VM / configuration | process-wide | per-thread | ratio | vs HotSpot |
|---|---|---|---|---|
| HotSpot JDK 25 | 248,666 KB | 248,666 KB | 1.00 | 1.00x |
| CratonVM ZGC (default), **before** | 627,662 KB | 72,423 KB | **8.67** | — |
| CratonVM ZGC, counter fixed | 487,189 KB | 487,189 KB | 1.00 | 1.96x |
| CratonVM G1, counter fixed | 363,057 KB | 363,057 KB | 1.00 | 1.46x |
| CratonVM Generational, counter fixed | 363,054 KB | 363,054 KB | 1.00 | 1.46x |
| **as shipped** (counter + narrowed collections, §5) — ZGC | 461,003 KB | 461,003 KB | 1.00 | 1.85x |
| as shipped — G1 | 336,750 KB | 336,750 KB | 1.00 | 1.35x |
| as shipped — Generational | 336,746 KB | 336,746 KB | 1.00 | **1.35x** |
| as shipped, Generational + `CRATONVM_COMPRESSED_OOPS=1` | 263,799 KB | 263,799 KB | 1.00 | **1.06x** |

**Both of the old record's headline numbers were wrong, in opposite directions.**
The 627 MB was a double-count layered on top of an under-count. The 72,423 KB
its last addendum proposed as the sound figure was the under-count alone — the
new process-wide counter reproduces that number to the byte with the
native-surface charging removed, which is how it was confirmed rather than
assumed.

The real multiplier is **1.35x** on Generational and G1 as shipped — not 2.5x
and not 0.3x — and 1.06x with compressed oops on. ZGC's 1.85x is a genuine
ZGC-specific footprint cost (it rounds small-object footprints up: a 24-byte
boxed `Integer` occupies 32) and not a counting error; its two counters agree on
it.

The class still **FAILS** on every configuration; the closest is Generational
with compressed oops at 263,799 KB against a 262,144 KB budget, 0.6% over. That
is a fair thing to report now and was not before: it is a figure two independent
counters agree on and that a retained-heap probe validates shape by shape.

---

## 4. What the old record's "next steps" turned out to be

**"Attribute the `ATNConfig` share directly … a 31% share that is inflated must
be inflated for a reason the shape probe does not reach."** There is no such
reason. `ATNConfig` is a plain 5-field object, and plain objects are laid out
exactly — `probes/AllocShapeTruth.java` reads user classes at `16 + 8N` on every
row. It is inflated for the only reason a plain object can be: 8-byte reference
slots and a 16-byte header against compressed oops and 12. The old record's own
shape probe *did* reach it; what it could not reach was that the aggregate it was
being compared against was fiction.

**"Reference width is not the main driver … on this test they close only 9% of
the gap."** REFUTED. That 9% was measured on the broken counter. Re-measured on
Generational:

| | parse | |
|---|---|---|
| compressed oops off | 363,054 KB | — |
| `CRATONVM_COMPRESSED_OOPS=1` | 290,118 KB | **closes 64% of the gap to HotSpot** |

Reference width is not merely *a* driver; it is the largest single one. (It
remains an opt-in, Generational-only gate — the audit for 4-byte reference slots
has only been done for that backend. Nothing here changes that.)

**"The collections' eager tables are independently worth fixing … Expect ~10-15%
of this test."** Right about the size, wrong about the mechanism. The map half
was already lazy on `dev` and had moved nothing: a fresh `new HashMap<>()` on
CratonVM already has `table = null`, byte-for-byte matching HotSpot's lazy
constructor, and an empty HashMap still cost **304 bytes** against HotSpot's 48.
The eager table was never the fixed cost. The **object** was — padded to a
synthetic-stub floor that is applied to real JDK classes and had drifted to five
times what the natives index.

Fixed here (one line per floor, and four validation runs — see the successor
record for why it needed four): HashMap 304 → 64 bytes, HashSet 576 → 128,
ArrayList 96 → 32, LinkedHashMap 384 → 224. Worth **7%** of this test, inside
the range the old record guessed for the wrong reason.

The ArrayList half does not apply in real-JDK mode at all: `new ArrayList<>()`
runs the JDK's own constructor, not `native_al_init`, so making that native lazy
changes nothing any measurement can see. Tried, measured as a null, reverted
rather than shipped unverifiable.

---

## 5. What is left, and where it went

The residue is object footprint. CratonVM's JDK collection classes were padded
to a synthetic-stub floor several times the slot count their natives index —
that half is **fixed here** (HashMap 304 → 64 bytes, HashSet 576 → 128,
ArrayList 96 → 32) and is worth 7% of this test. What is still open is `TreeMap`
and `IdentityHashMap`, still several times too wide by a **different and
unconfirmed** mechanism, and reference width, which is the largest single driver
left.

Both, with the measurements, the mechanism and how the fix was validated, in
[`docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md`](../fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md).

---

## 6. Harness notes

The old record's "Harness gotcha (Azure host only)" does **not** reproduce here:
all 243 entries of `apps/hib-suite-runner/common.args` resolve on this Windows
host. It remains an Azure-host build-completeness issue, not a VM defect.

`common.args` on this host points at the **`CratonVM1`** checkout, whose
`hibernate.properties` selects H2 in-memory. The `CratonVM` checkout's copy of
the same file selects PostgreSQL and cannot run this class without a live
database — worth knowing before concluding a class "fails on this host".

**Harness disposition, unchanged and still correct:** no `class-overrides.tsv` or
`known-benign-aborts.tsv` entry. No override can make a byte-budget assertion
pass, and `known-benign-aborts.tsv` never reclassifies `failed>0`. The class
stays a known CratonVM-specific FAIL.

---

## 7. Repro

```bash
# the instrument, on either VM (exits non-zero if a counter is off ground truth)
javac -d out probes/AllocCounterFidelity.java
java -Xmx256m -cp out AllocCounterFidelity 300000
cratonvm -XX:+UseGenerationalGC --Xmx 256m -c out AllocCounterFidelity 300000
```

```bash
# per-shape cost with retained heap beside it
javac -d out probes/AllocShapeTruth.java
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out AllocShapeTruth 100000
```

```bash
# the test's own window, both counters; then the test itself
cd apps/hib-suite-runner
sed -n '1,2p' common.args > cponly.args
javac @cponly.args -d out ../../probes/HqlParseAllocProbe.java
cratonvm -XX:+UseGenerationalGC --java-home "$JDK" --Xmx 2g -cp out @common.args HqlParseAllocProbe 2
cratonvm --java-home "$JDK" --Xmx 2g @common.args CratonRunner org.hibernate.orm.test.hql.HqlParserMemoryUsageTest
```
