# The FFM segment surface: 9 behavioural defects fixed, and the interface-as-a-class family sized

**Status: MEASURED AND PARTLY FIXED, 2026-08-29.** `apps/probes/FfmSegmentSweep.java`,
199 rows, run in both modes against HotSpot 25.0.4+7. **Every behavioural row is
now identical.** The 27 rows that still differ (47 under `--jdk-only`) are one
defect wearing five class names, and §4 sizes it rather than guessing at it.

This closes the `Arena`/`MemorySegment` row that
`HANDOFF-20260828-SCOPE.md` §4 listed as *"unclaimed, closest to L1"* — as far
as behaviour goes — and it re-opens it, precisely, as an identity question.

## 1. Why this probe exists

`the-definition-of-done-screen-run-for-the-first-time-20260828` §4 found the one
fabrication request of four that does **not** recover cleanly:

```text
Arena.ofConfined().allocate(16).getClass().getName()
  HotSpot   jdk.internal.foreign.NativeMemorySegmentImpl
  CratonVM  java.lang.foreign.MemorySegment          <- an interface, as an instance's class
```

That record measured ONE row and reasoned about the rest: *"`byteSize()` still
answers 16, so this is identity rather than function."* **The reasoning was
right about that row and wrong about the surface.** Asked properly — 199 rows
over the `Future`-shaped contract, the bounds, the refusals, the arenas, the
layouts and the object model — the same surface carried **nine behavioural
defects**, none of which is identity and none of which the DoD screen could see.

## 2. The nine, each with the row that measured it

| row | HotSpot | CratonVM (before) |
| --- | --- | --- |
| `arena.allocate(16).asReadOnly().set(JAVA_INT, 0, 1)` | `IllegalArgumentException` | **no-throw, and the write LANDED** |
| the read after it | `1020304` | `1` |
| `ValueLayout.JAVA_INT.order() == ByteOrder.nativeOrder()` | `true` | `false` |
| `arena.allocate(-1)` | `IAE: The provided allocation size is negative: -1` | no-throw |
| `arena.allocate(8, 0)` | `IAE: Invalid alignment constraint : 0` | no-throw |
| `arena.allocate(8, 3)` | `IAE: Invalid alignment constraint : 3` | no-throw |
| `MemorySegment.ofArray((byte[]) null)` | `NullPointerException` | no-throw |
| `s.asSlice(0, s.byteSize()).equals(s)` | `true` | `false` |
| `Arena.global().close()` | `UOE: Attempted to close a non-closeable session` | **no-throw** |

Three deserve naming.

### 2.1 `asReadOnly()` did not remove the one capability it exists to remove

`p67_segment_set_width` and `pe_segment_set_impl` both checked the SCOPE and
neither checked the read-only flag, so every write through a native
`asReadOnly()` view went straight to the raw pointer. `isReadOnly()` already
answered `true` on that receiver — the flag was right and nothing consulted it.

The heap path has had this check since `heap_segment_check_access`; the
raw-address path never did. Unlike the ALIGNMENT rule beside it — which that
function's comment explains cannot be transplanted, because a native segment's
real `maxByteAlignment` is not knowable from the carrier — the read-only flag IS
on the carrier.

**And the first fix was inert.** It went into
`phases_late/foreign_ffm.rs`, whose `set` registrations
`--dump-native-registry` reports as `owns_slot: false`; `panama.rs` owns them.
The rebuilt binary measured *exactly* the pre-fix transcript. Both halves now
carry the guard, because registration order is what picks between them and a
pair where only one half refuses is the half-fixed duplicate this file's history
keeps producing.

### 2.2 `ByteOrder` was minted per call, so `order()` could never equal `nativeOrder()`

`p67_byte_order_object` allocated a fresh `java.nio.ByteOrder` every time and
set its `name`. It satisfies every null check, prints the right string, and
fails the only test anyone writes — `ByteOrder` has exactly two instances and
every caller compares them with `==`. It now reads the class's own
`public static final` field and falls back to the mint only for a fabricated
stand-in with no statics to read.

This is `canonical_enum_constant`'s rule (`Thread$State`, `W7-93`'s
`StackWalker` options) reaching a fourth family. `ByteOrder` is not an `enum`,
but its constants are `static final` fields of its own type, which is the only
property that rule needs.

### 2.3 `Arena.global().close()` closed the global arena

`global()`, `ofAuto()` and `ofShared()` all minted the same two-slot carrier —
`p67_new_arena`'s only parameter was `confined`, which records the owner thread
and says nothing about closeability. Nothing could refuse. Not a missing
exception so much as a missing lifetime: every segment allocated from the global
arena is supposed to outlive everything, and a `close()` that succeeds is a
use-after-free waiting for its first reader.

The carrier gains a third slot, appended, so a narrower one minted elsewhere
reads as closeable — the pre-fix behaviour — rather than reading garbage.

### 2.4 And one comment that was wrong

`MemorySegment.equals` carried *"`MemorySegment` does not override `equals` in
the JDK — segment equality IS reference identity."* Measured:

```text
s.asSlice(0, 16).equals(s)                    true    <- two DIFFERENT objects
s.asSlice(0, 16) == s                         false
s.asSlice(0, 16).hashCode() == s.hashCode()   true
```

`AbstractMemorySegmentImpl` overrides both, over the base and the offset. It is
the second correction to that one line: the constant `false` it started as broke
reflexivity, identity fixed that and stopped one row short, and a caller that
de-duplicates segments before a bulk copy saw two where there is one.

## 3. What PASSED

The bulk of the surface was already right, and the pattern holds for a seventh
family: **the middle is correct and the perimeter was not.** Every value read
back at every width and both endiannesses; every bounds refusal including
straddling the end and negative offsets; slice arithmetic and slice-of-slice;
`fill`, `copy` and `mismatch`; the whole closed-arena refusal set
(`get`/`set`/`byteSize`/`close`-twice/`allocate`-after-close); zero-length
segments; `ofBuffer`; `heapBase()`; and every layout's `byteSize`,
`byteAlignment`, `elementCount` and struct sizing.

## 4. The residual, sized rather than guessed

**25 rows in compatible mode, 45 under `--jdk-only`, down from 27/47: the
`Arena` family is CLOSED (2026-08-29) and the rest is one defect.** All of what
is left is `class`, `superclass` and `isInterface` -- 13, 10 and 2 rows.

```text
                       compatible                                    --jdk-only
MemorySegment    cratonvm.internal.foreign.MemorySegmentImpl   java.lang.foreign.MemorySegment
Arena            java.lang.foreign.Arena                       java.lang.foreign.Arena
ValueLayout$OfInt java.lang.foreign.ValueLayout$OfInt           (same)
StructLayout     java.lang.foreign.StructLayout                (same)
SequenceLayout   java.lang.foreign.SequenceLayout              (same)
```

against HotSpot's `jdk.internal.foreign.NativeMemorySegmentImpl`,
`HeapMemorySegmentImpl$OfByte`, `ArenaImpl`,
`layout.ValueLayouts$OfIntImpl`, `layout.StructLayoutImpl`,
`layout.SequenceLayoutImpl`.

**Both modes are wrong and they are wrong differently**, which is the useful
part and is new:

* **compatible INSTANTIATES a fabrication** — `cratonvm.internal.foreign.
  MemorySegmentImpl`. Any program that touches FFM therefore has
  `compatibility_classes > 0`, which is the definition-of-done predicate.
* **strict refuses it and lands on the INTERFACE**, so `getClass().isInterface()`
  is `true`, `getSuperclass()` is `null` and `Modifier.isAbstract` is `true` —
  an object the Java object model does not contain. Four of the five families
  above are already in that state in BOTH modes, because only the segment
  carrier has a fabricated stand-in to fall back from.

What the probe establishes that reasoning could not: the damage is bounded to
identity. `isInstance`, `instanceof`, `isAssignableFrom` and a class-keyed
`HashMap` round-trip all still answer correctly on the interface-classed
objects, on both VMs. So this breaks a `getSuperclass()` walk, a
`getClass().getName()` log line, and any code that switches on the concrete
implementation class — not the ordinary uses.

### 4.1 The reason given here for stopping was wrong. WITHDRAWN 2026-08-29

This section said naming the real classes "means moving the whole segment model
onto the JDK's", because every accessor addresses a raw slot index. That is a
statement about the accessors, and it was not checked against the file it is
about.

**`p67_memory_session`, two hundred lines above the arena in the same file,
already mints the REAL `jdk/internal/foreign/MemorySessionImpl` and resolves its
fields BY NAME** (`p67_session_slots`), with a synthetic fallback for the carrier
that has no real class behind it. The primitive this section says does not exist
is the one the neighbouring family runs on. `--dump-native-registry` was saying
it too, and I had the dump open: `ArenaImpl`, `NativeMemorySegmentImpl` and
`ValueLayouts$OfIntImpl` each carry registered natives with **zero invocations**
-- a destination already wired, with nothing minting anything that reaches it.

**`Arena` is done.** `Arena.ofConfined().getClass()` answers
`jdk.internal.foreign.ArenaImpl` in all three arms.

### 4.2 The recipe, and what each step costs when it is skipped

Three things move together. Both attempts where they did not are recorded
because the failures are the useful part:

| Step | What it is | What skipping it cost |
|---|---|---|
| The **class** | mint the real impl, not the interface | -- |
| The **slots** | resolve by NAME from the receiver's class, one shared resolver, synthetic fallback | `panama.rs` kept a hard-coded index 1 for the session; its own comment already warned that "open-coding the index here is what let the two files drift out of step" |
| The **whole family's registrations** | every INSTANCE method, on the impl class too | moved `close`/`scope`/`allocate`, left the ten `allocateFrom` shapes: dispatch fell through to JDK default bytecode and `RJdkForeign`'s downcall **SIGSEGV**'d in `heap_read_bytes`, both modes |

And spell the class names as **literals** at the registration site.
`for arena_cls in [arena, P67_ARENA_IMPL]` reddened `registrar_drift` with
`STALE BASELINE -- 1 recorded drift pair no longer drift`: that scanner reads the
call TEXT, so a variable and a const read to it as no registration at all, while
the registration was live the whole time.

### 4.3 A latent defect the move surfaced

`pe_arena_allocate_impl` discriminated the two rival arena layouts **by width**
(`object_num_fields > 3`), under a comment saying the reads are "gated on the
width that makes them meaningful". A width is not an identity. The new carrier
is four slots wide, so it took the `--synthetic-jdk` branch, read this model's
`open` flag as that model's `closed` flag, and every `arena.allocate` threw
`Arena is closed` -- **0 of 199 rows, both modes**. Two producers were minting
ONE class with two different layouts; now that they mint different classes, the
test asks the class. See `two-producers-of-one-carrier-class-is-a-failure-family`.

### 4.4 What the remaining four families cost, measured

Instance-field counts from `javap -p` on the JDK 25.0.4+7 image, superclass
fields included -- these are the layouts a by-name resolver has to land on:

```text
  jdk.internal.foreign.ArenaImpl                     2   DONE 2026-08-29
  jdk.internal.foreign.NativeMemorySegmentImpl       4   (min + length, readOnly, scope)
  jdk.internal.foreign.HeapMemorySegmentImpl$OfByte  5   (offset, base + the same three)
  jdk.internal.foreign.layout.SequenceLayoutImpl     5   (elemCount, elementLayout + byteSize, byteAlignment, name)
  jdk.internal.foreign.layout.ValueLayouts$OfIntImpl 6   (+ carrier, order, handle)
  jdk.internal.foreign.layout.StructLayoutImpl       6   (kind, elements, minByteAlignment + the same three)
```

None of them is large -- **but the field count is the wrong axis, and it is the
axis I first priced them on.** What the move costs is step 2 of the recipe: the
number of places that read those fields by a RAW INDEX, each of which has to go
through the resolver or become the next `Arena is closed`. Counted by walking
every function in the two files and matching `get_field(x, N)` / `set_field(x, N)`:

```text
                    functions   raw-index accesses   already by-name
  layout family        19              37                  2
  segment family       17             112                 19
  (arena, for scale)    3               4                  0
```

**So the layout families really are the smaller half -- by 3x, not by the
handful the field counts suggested -- and the arena was smaller than either by
an order of magnitude.** That is why it went first and why it was the right
place to learn the recipe; it is not evidence that the others are the same size.

Two things the count does not show, both found while sizing it:

* **The layout accessors already SHAPE-GUESS between carriers**, which is the
  same species as the width test in §4.3 and has to be replaced in the same
  change, not after it. `p67_layout_name_value` picks slot 2 or slot 3 by asking
  whether slot 0 holds an `Int`; `p67_layout_byte_alignment` reads slot 1 and
  falls back to slot 0 when it is not a `Long`. Value, group and sequence
  carriers all pass through them. Move one family and the others' reads move
  with it, so the layout half is **all thirteen carriers or none** -- nine
  `ValueLayout`s plus struct, sequence, union and padding.
* **Some of the work is already done, and it argues the design is right.**
  `p67_layout_is_little` resolves the real `order` field BY NAME and separates
  real from fabricated by asking `declared_fields` for `carrier`/`order`;
  `p67_layout_carrier_name` already answers for both spellings; and
  `ValueLayouts$OfIntImpl` carries 11 of the interface's 17 natives, needing
  only `byteSize`, `byteAlignment`, `byteOffset` and `varHandle`. `StructLayoutImpl`
  and `SequenceLayoutImpl` carry none of theirs (0 of 7, 0 of 10).

The segment family remains the one that clears `compatibility_classes > 0`,
since it is the only one with a fabricated stand-in -- and it is now measured as
the most expensive of the three, not merely the widest by native count.

## 5. Reproduce

```bash
javac -d apps/probes/out apps/probes/FfmSegmentSweep.java apps/probes/FfmMsgProbe.java
for P in FfmSegmentSweep FfmMsgProbe; do
  "$JDK/bin/java" -cp apps/probes/out $P > hs-$P.out 2>/dev/null
  cratonvm --java-home "$JDK"            -cp apps/probes/out $P > cc-$P.out 2>/dev/null
  cratonvm --java-home "$JDK" --jdk-only -cp apps/probes/out $P > jo-$P.out 2>/dev/null
  diff hs-$P.out cc-$P.out; diff hs-$P.out jo-$P.out
done
```

`FfmSegmentSweep`'s last line is `rows 199 DONE FfmSegmentSweep`; check it before
reading any diff. The first run of this probe died at row 146 **on HotSpot too**
— a `JAVA_INT` write into a `byte[]` heap segment, which is an alignment refusal
and was the probe's bug, not the VM's.

`FfmMsgProbe` is the MESSAGE of every refusal added here, measured on the oracle
before any of them was written, including the space before the colon in
`"Invalid alignment constraint : 0"`.
