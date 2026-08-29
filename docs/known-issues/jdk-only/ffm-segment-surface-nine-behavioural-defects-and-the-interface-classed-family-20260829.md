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

27 rows in compatible mode, 47 under `--jdk-only`, and they are one defect:

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

**Not fixed here, and the reason has been checked rather than assumed.** Naming
the real classes is not a rename: `NativeMemorySegmentImpl` and
`HeapMemorySegmentImpl$OfByte` have their own field layouts, and every accessor
in `panama.rs` addresses this VM's six-slot carrier by raw slot index. Adopting
them means moving the whole segment model onto the JDK's, which is the same
answer the DoD screen gave and the same shape as
`fabricating-an-abstract-class-trades-an-npe-for-an-abstractmethoderror`. The
layout families (`ValueLayouts$OfIntImpl` and friends) are the smaller half and
may be tractable on their own; nothing here has measured that.

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
