# JDK collection objects are wider than their fields, because a synthetic-stub floor is applied to real classes

**Status:** The two drifted floors are **fixed and shipping**. Two more classes
(`TreeMap`, `IdentityHashMap`) are still several times too wide by a **different
and unconfirmed** mechanism, and reference width remains a separate, larger
subject. Those are what is open.

**Successor to**
`docs/internal/retired/hibernate-orm-hql-parser-memory-overhead-RETIRED-20260911.md`,
which fixed the allocation counter and found this underneath it. Read that
record's §4 for why the eager-bucket-table hypothesis this replaces was wrong.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/hibernate-hql-parser-memory-0cb182` off `dev@5bc72dda3`.

## The measurement

`probes/AllocShapeTruth.java` reads RETAINED HEAP per instance, so these are
object footprints and not counter readings — which matters, because the record
that led here spent three revisions on counter readings.

| class | declared instance fields | HotSpot | CratonVM before | CratonVM now |
|---|---|---|---|---|
| a user class, 0 refs | 0 | 15.7 | 16.0 | 16.0 |
| a user class, 4 refs | 4 | 32.2 | 48.0 | 48.0 |
| a user class, 8 refs | 8 | 49.2 | 80.0 | 80.0 |
| a user class, 8 ints | 8 | 48.4 | 48.0 | 48.0 |
| `java.util.ArrayList` | 3 | 24.5 | **96.0** | **32.0** |
| `java.util.HashMap` | 8 | 49.1 | **304.0** | **64.0** |
| `java.util.LinkedHashMap` | 12 | 65.1 | **384.0** | **224.0** |
| `java.util.HashSet` | 1 (+ its map) | 64.0 | **576.0** | **128.0** |
| `java.util.TreeMap` | ~10 | 48.1 | **352.0** | **352.0 — still open** |
| `java.util.IdentityHashMap` | ~4 | 315.1 | **584.0** | **584.0 — still open** |

**User classes were never the problem**: `16 + 8N`, exactly, every row, before
and after. The 1.5x on reference-bearing rows is reference width (8-byte slots
and a 16-byte header against compressed oops and 12) — uniform, expected, and a
separate subject.

## The mechanism

`classloading/src/class_manager.rs`, where a class is defined:

```rust
let stub_total = stub_parent_fields + stub_instance_count;
let num_total_fields = num_total_fields.max(stub_total);
```

`stub_instance_count` comes from `synthetic_stub_fields(name)`, a table of
per-class slot floors that exists for **synthetic-JDK mode**, where those stub
classes *are* the layout and native `<init>` methods write to synthetic indices
no real class declares. Without the floor the object would lack slots for those
writes.

The floor is applied in **real-JDK mode too**, where the real class already
declares everything the natives need, and it is applied as a `max`, so it only
ever ratchets a class wider. `build_compact_layout` then fills the padding with
8-byte reference slots. An empty `java.util.HashMap` occupied 36 slots — with
`table`, `entrySet`, `keySet` and `values` all null, so nothing on the side
explained it. The 304 bytes was the object.

Two floors had drifted far past what the natives index, and the code said so in
its own comments:

| class | comment | code was | what the natives index |
|---|---|---|---|
| `HashMap` / `HashSet` / `Hashtable` / `EnumMap` / `ConcurrentHashMap` | "= 3 fields (buckets, size, capacity)" | `instance_fields(16)` | `MAP_FIELD_BUCKETS=0`, `MAP_FIELD_SIZE=1`, `MAP_FIELD_CAPACITY=2`; `HS_FIELD_MAP=0`; `CHM_FIELD_SEGMENTS=0` and its mask `=1` |
| `ArrayList` / `Vector` / `Stack` / `CopyOnWriteArrayList` | "= 2 fields (data, size)" | `instance_fields(4)` | `AL_FIELD_DATA=0`, `AL_FIELD_SIZE=1`, `AL_NUM_FIELDS=2` |

Comment and code disagreeing in the same direction in two independent entries is
the signature of a floor raised defensively and never brought back down. Both
are now what their comments always said: 3 and 2.

## What it bought

| | parse allocation | budget 262,144 KB |
|---|---|---|
| counter fix only, Generational | 363,054 KB | FAIL by 38% |
| + narrowed floors | 336,746 KB | FAIL by 28% |
| + `CRATONVM_COMPRESSED_OOPS=1` | **263,799 KB** | FAIL by **0.6%** |

(That is `HqlParserMemoryUsageTest`, the workload the parent record was opened
against. It still fails. HotSpot allocates 248,666 KB on the same parse, so the
first row is 1.46x and the last is 1.06x.)

## How it was validated, and why that took four runs

Lowering a floor fails **silently**: an out-of-range `set_field` is dropped
rather than raised, so a lost write shows up later as a collection that quietly
has the wrong contents. A green suite is therefore not by itself evidence —
the evidence has to reach the modes and the slots.

* **Real-JDK, 300 Hibernate ORM classes**, `run-hib.sh --category passed --count
  300`: byte-for-byte identical `status/found/ok/failed/aborted/skipped` against
  the unchanged build. 300 PASS, 0 non-PASS, both arms.
* **`regression-suite/run.sh`**: 92/92, identical with and without.
* **`probes/CollectionSlotFloor.java`** (written for this): exercises every
  collection family whose floor is in that table, through growth past the initial
  table, removal, iteration, `toArray` and — for `LinkedHashMap` — insertion
  order, which is what slots 3 and 4 hold and the quietest thing to lose. PASS in
  real-JDK mode.
* **Synthetic-JDK mode**, which `regression-suite` does not cover and which needs
  its own binary (`--features synthetic-jdk`): the same probe produces an
  **identical set of eight mismatches** with and without the change. Those eight
  are pre-existing synthetic-mode gaps in `TreeMap.keySet()/entrySet()`,
  `IdentityHashMap` and `LinkedHashSet` — worth their own record, unrelated to
  the floors, and confirmed pre-existing by building the unchanged tree with the
  same feature and running the same probe.

**Raising a floor in that table is free. Lowering one needs that evidence
again** — the real-JDK arm alone would have proved nothing about the mode the
floors exist for.

## What is still open

1. **`TreeMap` (352 bytes) and `IdentityHashMap` (584)** are still 7x and ~2x
   HotSpot, and their own floors are 3 and unset — so the floor table is *not*
   the cause. The likely candidate is the `max(old, new)` preservation on
   synthetic-stub upgrade (`class_manager.rs` ~10659 / ~11043), a second ratchet
   on the same field: a class first fabricated wide and later upgraded to its
   real definition keeps the wide count. **Not confirmed.** Confirming it is the
   first step, and `probes/AllocShapeTruth.java` is the instrument.
2. **`HashSet`** inherits its backing map's improvement, but its own object is
   still wider than one field needs; it is in the same family as (1).
3. **Reference width.** 8-byte slots and a 16-byte header are the largest single
   remaining driver on the parse — `CRATONVM_COMPRESSED_OOPS=1` closes 64% of the
   gap to HotSpot. It stays opt-in and Generational-only: only that backend's
   scan and relocation paths have been audited for 4-byte reference slots.
   Widening that audit is its own piece of work and is not tracked here.

## Repro

```bash
javac -d out probes/AllocShapeTruth.java probes/CollectionSlotFloor.java
java -Xmx3g -cp out AllocShapeTruth 100000                       # HotSpot
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out AllocShapeTruth 100000
cratonvm --Xmx 2g -c out CollectionSlotFloor                     # real-JDK: clean
```

```bash
# the arm that matters when touching a floor; needs its own binary
cargo build --release -p cratonvm-cli --bin cratonvm --features synthetic-jdk
cratonvm --synthetic-jdk --Xmx 2g -c out CollectionSlotFloor     # expect the documented eight
```
