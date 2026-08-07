# `monitorexit` erased the object header's quartet — FIXED 2026-08-07

**Status: FIXED.** One line in `vm/src/threading/monitor.rs`
(`try_thin_unlock`), plus a two-bit widening of `MARK_QUARTET_MASK` in
`types/src/heap_types.rs` that the regression test for the first fix uncovered.

This is the defect behind two separately-filed pages and one whole blocked
suite:

* the retired `compact-ref-field-layout-corrupts-filechannel-filelock-20260807`
  write-up — `FileChannel.tryLock()`/`lock()` failed on every file-backed
  database, so no persistent H2 database opened at all on dev tip;
* **every Spring Boot test class (1975) failed at JUnit discovery** on dev tip
  — found while trying to re-run the two full-suite G1 comparisons, and not
  previously filed;
* `CRATONVM_COMPACT_REF_FIELDS=0` was a complete workaround for both, which is
  why both were read as compact-layout bugs.

## The bug

Since `d7965af6a` (*"HEADER_SIZE 24 -> 16"*) an object's `kind`,
`element_type`, `gc_flags` and `gc_age` — the **quartet** — live in mark-word
bits 48..63 rather than in their own header fields. Every mark-word constructor
was updated to carry them across a state change:

```rust
pub fn make_thin_locked(prev: u64, ...) -> u64 { Self::quartet_of(prev) | MARK_THIN_LOCKED | ... }
pub fn make_inflated(prev: u64, ...)    -> u64 { Self::quartet_of(prev) | ... | MARK_INFLATED }
pub fn make_neutral_hashed(prev: u64, ...) -> u64 { Self::quartet_of(prev) | MARK_NEUTRAL | ... }
```

`try_thin_lock` even carries a comment explaining why its own literal compare
had to become a masked one. Its unlock counterpart was missed:

```rust
let (new, ret) = if recursion == 0 {
    // Last release → return to NEUTRAL.
    (types::MARK_NEUTRAL, None)      // <-- the whole quartet, gone
```

`MARK_NEUTRAL` is `0b00`. **The first `synchronized` block on any object
destroyed that object's kind, element type, GC flags and age.**

Fixed to `ObjectHeader::quartet_of(cur) | types::MARK_NEUTRAL`.

## Why it presents as a layout bug

Losing `GC_FLAG_COMPACT` is the loudest of the four. Readers that honour the
per-object header — `gen_heap::compact_field_slot`, hence every native
`get_field` — then decode the object's packed 8-byte reference fields as
16-byte legacy `Value` cells:

```
ERROR cratonvm::gc::guard: gen_heap::read_slot: corrupt Value cell
  (out-of-range discriminant) — returning null instead of a UB-on-match Value.
```

`CRATONVM_DBG=cellcorrupt` on the Spring Boot repro names the victim and shows
the two packed references being read as one cell — `raw0` is the backing
collection, `raw1` is the object itself, which is `SynchronizedCollection`'s
`mutex = this`:

```
[CELLCORRUPT] holder=0x200c2c45078 class_id=450
  class=java/util/Collections$SynchronizedSet num_slots=2 gc_flags=0x0 index=0
  raw0=0x00000200c2c069a8 raw1=0x00000200c2c45078
[CELLCORRUPT]   target-header: class=java/util/LinkedHashSet
```

`gc_flags=0x0` on an object whose body is unmistakably compact is the whole
finding. The retired FileChannel page read the same `gc_flags=0x0` and inferred
"**allocated legacy, written compact**" — a per-class writer gate racing a
per-allocation decision. That hypothesis is **refuted**: `CRATONVM_DBG=compact-legacy`
prints one line for every allocation that actually takes the legacy fallback and
prints nothing for either victim class, so both objects were allocated *compact*
and un-compacted afterwards. The writers it named are not implicated, and the
`plan_object_alloc` seam it proposed to close is not what fails here.

Bytecode `getfield`/`putfield` keep working on a de-flagged object precisely
*because* they gate per class: writer and reader stay consistently compact. Only
the header-honouring readers diverge. That is why the failure looks like "the
natives are wrong" from one side and "the layout is wrong" from the other.

## Reproducing — eight lines, no JUnit, no H2, `--nojit`

`probes/MonitorQuartetProbe.java`:

```java
Set<String> s = Collections.synchronizedSet(new LinkedHashSet<>());
System.out.println("before = " + s.iterator());   // java.util.HashMap$KeyItr
synchronized (s) { }                              // quartet wiped here
System.out.println("after  = " + s.iterator());   // null, on a broken build
```

Broken build prints `after = null`; fixed build prints an iterator. The
addresses are identical on every run — this is not a race.

Ranked by blast radius, the same call is why the Spring Boot suite died:
`AbstractTestDescriptor.children` is a synchronized set and
`EngineDiscoveryResultValidator.getCyclicGraphInfo` walks
`descriptor.getChildren().iterator()`, so **`TestEngine with ID 'junit-jupiter'
failed to discover tests` / `NullPointerException: Cannot invoke
"java.util.Iterator.hasNext()"` on all 1975 classes**, under the default
collector, JIT or not.

## The second bug the regression test found

Writing the obvious test — set a flag and an age, lock, unlock, assert the
quartet is unchanged — failed at the *lock*, with the mark word coming back
`INFLATED`. `MARK_QUARTET_MASK` was `0x3FFF << 48` (bits 48..61) while
`AGE_SHIFT` is 60 with 4 bits (60..63): **the mask stopped two bits short of its
own last field.** Its doc also called that "13 bits"; `0x3FFF` is 14.

For any object whose age reached 4 (bit 62):

1. `try_thin_lock` tests `cur & !MARK_QUARTET_MASK != MARK_NEUTRAL` to mean
   "unlocked and unhashed". Age bit 62 sits outside the mask, so the test could
   never succeed and **every `synchronized` on an aged object inflated a
   `Monitor`** — an allocation and a registry entry on what is meant to be the
   uncontended fast path.
2. Every `quartet_of(prev)` carry dropped the top two age bits, so the first
   lock or identity hash of an age-12 survivor **reset its age to 0** and
   restarted tenuring. Objects that are hashed or locked each cycle — every
   `HashMap` key, every lock — could not reach `PROMOTION_AGE`.

Widened to `0xFFFF << 48`. Safe against every other user of the word for the
reason the surrounding block comment already gives: `make_inflated` and
`make_forwarded` both assert `plausible_heap_pointer`, capping a payload pointer
at `2^47 - 1`; the NEUTRAL hash occupies bits 2..33 and the thin-lock
owner/recursion bits 2..42.

## Regression tests

* `types::heap_types::quartet_layout_tests::quartet_covers_every_quartet_field`
  — each of `kind`/`element_type`/`gc_flags`/`gc_age` must lie wholly inside
  `MARK_QUARTET_MASK`, so a later field cannot outgrow it silently.
* `…::quartet_does_not_overlap_any_state_payload` — the mask must not reach into
  the state tag, the hash, or the thin-lock fields, stated from the other side
  so widening it further cannot eat a payload.
* `…::every_representable_age_survives_a_quartet_carry`.
* `vm::threading::monitor::tests::lock_unlock_round_trip_preserves_the_header_quartet`
  and `…::recursive_lock_unlock_preserves_the_header_quartet`.

## Verification

| probe | before | after |
|---|---|---|
| `probes/CompactLayoutFileLockProbe` (`--nojit`, the retired FileChannel page's own repro) | `PROBE-FAILURES=1` + corrupt-cell guard | `PROBE-OK` |
| `Collections.synchronizedSet(…)` iterator after `synchronized` | `null` | iterator |
| `EngineDescriptor.getChildren().iterator()` on the JUnit 6 classpath | `null` | iterator |
| Spring Boot classes, default collector | 14/14 CRASH at discovery | see the G1 comparison pages |

`gc` 984 tests, `types` 519, `vm::threading::monitor` 51 — all green.

## One more site, same conflation

`G1Collector::parallel_evacuate`'s forward-retirement loop also stored a bare
`MARK_NEUTRAL` over from-space copies, with a comment reasoning only about lock
state ("there is no lock state left to preserve on a dead copy"). `kind` and
`element_type` are not lock state — they are what every linear region walker
sizes a from-space object from, and this store runs before Phase 5 zeroes the
region. Also fixed to carry the quartet. Reachable only under
`CRATONVM_G1_PARALLEL_EVAC`, so it was not part of either reported failure.
