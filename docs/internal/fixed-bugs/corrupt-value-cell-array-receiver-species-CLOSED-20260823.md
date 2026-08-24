# The unreproduced KafkaMetrics corrupt cell: the species is closed at the accessor

**Status: CLOSED 2026-08-23 — the SPECIES, not the sighting.** The one cell that
page recorded is still unreproduced, and this page does not claim otherwise.
What changed is that the shape it had can no longer be produced: a plain-object
field access whose receiver is an ARRAY is refused at all four heap accessors
and reported through the corrupt-cell chain, so a door that makes that mistake
now names itself the first time instead of once in 1975 classes.

Supersedes
`known-issues/gc/corrupt-value-cell-one-unreproduced-hit-in-kafkametrics-20260822.md`.

---

## 1. What the open page was holding

One `cratonvm::gc::guard` hit, in one class, in one arm of a two-arm Spring Boot
sweep on 2026-08-22:

```text
ERROR cratonvm::gc::guard: zgc::get_field: corrupt Value cell (out-of-range
discriminant) slot=0x2c8d3d7c000
  raw0="0x0079786f72502f74"  raw1="0x000000010000000c"
```

`raw0` little-endian is `74 2f 50 72 6f 78 79 00` — **`"t/Proxy\0"`**, UTF-8
text. The page's own most actionable line was that this is "the shape of a
name/descriptor buffer reached through an object field index", and that the read
did not come through any instrumented door.

Its three follow-ups were: widen the instrument, chase the `"t/Proxy"` lead, and
remember that a quiet run is not a correct run. The first was done on
2026-08-23 (doors, the interpreter's own `getfield`, and a safepoint backstop,
plus an exit census). This page does the second, and turns the third from an
argument into a counter.

## 2. `"t/Proxy"` is arithmetic, not a coincidence

`java/lang/reflect/Proxy` is 23 characters. Index 16 onward is `t`, `/`, `P`,
`r`, `o`, `x`, `y` — then the 24th byte, past the end of a 23-byte body, reads
as padding zero. That is `raw0`, exactly.

```text
  j a v a / l a n g / r e f l e c t / P r o x y
  0 1 2 3 4 5 6 7 8 9 …           16 …      22
                                   ^^^^^^^^^^^ + NUL  = raw0
```

A 16-byte `Value` cell at slot index 1 sits at byte offset `HEADER_SIZE + 1*16`
= 16 of the receiver's body. So the cell was **slot 1 of a 16-byte stride over
the `byte[]` that backs the String `"java/lang/reflect/Proxy"`** — the read
decoded packed element data, and `raw1` is whatever the allocation's tail or its
neighbour's header held.

## 3. Why the index check does not catch it

Every backend's `get_field` bounds the index against `num_slots`, and every
backend's `alloc_array` MIRRORS THE LENGTH INTO `num_slots`:

```rust
ObjectHeader::new(class_id, ObjectKind::Array, element_type, len, len)
//                                                            ^^^  ^^^
//                                              array_length  num_slots
```

So `new byte[23]` reports twenty-three "fields". Every index a real element has
passes the bounds check, while the byte offset it computes — 16 bytes per slot
against 1 byte per element — names neither that element nor, past index 1, any
part of the allocation. The same arithmetic runs 2x past for a reference array,
whose elements are bare 8-byte pointers (`REF_ELEMENT_SIZE`), which is the
`String[]` defect closed on 2026-08-22.

**Two doors reach it, and they screen on different things:**

| species | the test that admits an array | record |
| --- | --- | --- |
| a class-id fast path | a reference array has no class of its own, so it carries its COMPONENT's class id: `class_id_of(new String[2])` answers `java/lang/String` | `corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`, closed at four doors with `is_plain_string` |
| a SHAPE probe | `num_fields(obj) < 2` is not an array screen — a `byte[]` of length ≥ 2 walks through it | this page |

`vm_object::java_string_value_and_coder` is the second one. Its guard says, in
its own words, "A String always at least 2 slots (value + coder), so anything
smaller cannot be a String" — true, and it reads like an array screen without
being one.

## 4. The fix is at the accessor, and it is one rule

`heap::refuse_array_receiver_field_access` — `heap`, `gen_heap`, `g1` and `zgc`
all call it. A plain-object field access whose receiver is an array is refused
(null read / dropped write) and reported, and the raw words are read back for
the report ONLY when the cell they name lies inside the array's own body, since
the striding access it refuses is frequently outside the allocation entirely —
which is the other half of what it fixes.

Screening there is what makes it ONE rule instead of one rule per door. The
doors keep their fast paths; a door that forgets the rule gets a named refusal
instead of a silent stride. Three doors were also fixed directly, because a
reader that can answer "not a String" for free should not make the heap say it:

* `vm_object::java_string_value_and_coder` — the `num_fields` probe above;
* `gc_and_alloc`'s `is_reference_shaped` / `is_queue_shaped` — a `Reference[]`
  passes BOTH their tests (length mirrored into `num_slots`, and the component's
  class id answering `is_subclass_of(java/lang/ref/Reference)`).

## 5. It reports through the corrupt-cell chain, and that is the point

The refusal calls `cell_census::note_decoded()` — the counter the VM-side
producer reporter compares across a field read to decide "that read tripped the
guard" — so an array-receiver refusal arrives with a door, a receiver, and a
Java stack under `CRATONVM_DBG_CORRUPT_CELL`, for free.

It also calls `note_array_receiver()`, counted and printed separately:

```text
[corrupt-cell] array_receiver=0
[corrupt-cell] decoded=0 reported=0 — armed, and the guard did not fire
```

Separately, because they are different measurements. **`decoded` is a floor.**
The `gc::guard` report fires only when the striden bytes happen to form an
out-of-range discriminant; bytes that form a valid tag are invisible to it — the
open page said as much and it is why one hit per 1975 classes was a floor and
not a count. `array_receiver` fires on every occurrence. A sweep reporting
`decoded=0 array_receiver=7` has found seven producers the guard could not see.

The Spring Boot runner reads both back into its own summary and stdout.

## 6. Measured

**Unit** (`gc/src/heap.rs`): a refused read answers null, a refused write leaves
every element intact — asserted afterwards, which is what separates "refused"
from "wrote somewhere harmless" — and both the local counter and the exit census
move. Reference arrays and primitive arrays, in-body and out-of-body indices.
1689 `cratonvm-gc` lib tests and 2608 `cratonvm-vm` lib tests pass.

**Gate** (`regression-suite/RStringBuilderContent`): the existing array group
covered the class-id half. It now also covers the shape-probe half, over the
`byte[]` of `"java/lang/reflect/Proxy"` itself — its length is asserted not to
be a field count, its bytes 16..24 are asserted to be `"t/Proxy"`, and it is
carried through the rendering doors, a `HashMap` round trip, a `HashSet`, and
the identity-vs-content `equals`/`hashCode` contract.

**Both new gate groups PASS on the pre-fix `dev` binary as well.** They are a
forward gate, not a reproduction, and saying so is the point: nothing in the
in-tree corpus reaches the un-screened path today, which is consistent with a
cell seen once in 1975 classes and never again.

**Probe** (`probes/ArrayReceiverProbe`): every shape-probing door — rendering,
identity, five collection shapes, the `Arrays` helpers — over eight array
receivers, compared against HotSpot. Byte-identical on `dev` and on the fix,
under all three collectors, with `array_receiver=0`.

**Sweep**: the full Spring Boot suite on Windows, the same corpus and harness
the sighting came from, census armed:

```text
shard 1 (classes 1-500)     0 decoded, 0 array-receiver refusals, 499 of 500 logs armed
```

(The one unarmed log is a HANG killed before its shutdown trailer — a blind spot
worth stating rather than rounding off.)

## 7. What is and is not claimed

* **Claimed:** a `byte[]` body can no longer be read as a `Value` cell, whatever
  door asks — which is the exact shape the sighting had. The class-id species
  and the shape-probe species are both screened, at the accessor and at the
  doors, and any recurrence is counted rather than caught by luck.
* **Not claimed:** that the KafkaMetrics cell has been reproduced, or that its
  particular door has been named. It has not. A second reading of the same
  evidence remains open — that the receiver was not an array at all but an
  INTERIOR pointer to the array's data treated as an object base, in which case
  the "header" would be the first sixteen name bytes and the cell would be slot
  0 of that. That shape is a receiver-validity defect, not a kind defect; ZGC's
  `audit_access_receiver` (`CRATONVM_DBG_ZGC_CORPSE`) is the instrument for it
  and it is the thing to arm if the cell ever comes back.
* **Not claimed:** that a quiet sweep proves correctness. It does not, and the
  reason this page can be closed anyway is that the fix is a screen at the
  accessor rather than a fix at one door — its correctness does not depend on
  the sweep finding anything.

## Reproduce (the species, not the sighting)

```bash
cratonvm --java-home "$JDK" -cp . ArrayReceiverProbe          # agrees with HotSpot
CRATONVM_DBG_CORRUPT_CELL=1 cratonvm ... ArrayReceiverProbe   # array_receiver=N
```

and, for any workload:

```bash
CRATONVM_DBG_CORRUPT_CELL=1 cratonvm ... 2>&1 | grep 'corrupt-cell'
```

`array_receiver=0` is a measurement. An absent line is not.
