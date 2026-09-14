# G6-1 — the FFM surface measured, and the merge's three questions settled

**Status:** FIXED-ON-ORACLE, **NOT MEASURED ON A CRATONVM BINARY.**
**Provenance:** every oracle row below is **MEASURED** on HotSpot
25.0.3+9-LTS (Temurin, `openjdk version "25.0.3" 2026-04-21 LTS`, build
`25.0.3+9-LTS`). Every claim about CratonVM's *previous* behaviour is
**SOURCE-VERIFIED** (read out of the code being replaced), never measured.
Every claim about CratonVM's *new* behaviour is **PREDICTED**.

> **Read this first.** This lane could not build or run CratonVM — the tree was
> mid-merge and the orchestrator owns the target directory. So this record is
> exactly the shape HANDOFF-20260814 §2 warns about for waves A–F: good
> analysis with a real oracle behind it, and **no VM has ever agreed with any
> of it.** The one thing that is different is the denominator: **450 oracle
> rows** were transcribed (316 + 52 + 54 + 28 across the four transcripts,
> plus the `P5`/`P6`/`P7` follow-ups), not three — HANDOFF §4's "sweep the
> family, not the row".

Probes (ASCII labels only, per HANDOFF §7):
`scratchpad/ffm/{FfmProbe,FfmProbe2,FfmProbe3,FfmProbe4,P5,P6,P7}.java`,
transcripts `out.txt`, `out2.txt`, `out3.txt`, `out4.txt`.

Files owned and changed: `native-builtins/src/panama.rs`,
`native-builtins/src/phases_late/foreign_ffm.rs`. Nothing else.

---

## 0. The headline

The merge lane left three named questions. All three are settled, and settling
them found **nine divergences**, four of them in code the merge itself had just
written from a PREDICTED reading of the JDK.

| # | question / find | verdict |
|---|---|---|
| 1 | `maxByteAlignment()` for a heap segment | **settled — it is the element type, not 8** (§1) |
| 2 | `spliterator`/`elements`/`toArray` on a heap receiver, untested | **tested, and `toArray` was returning zeros** (§2, §3) |
| 3 | the deleted group-layout family | **confirmed gone; its last survivor was dead and is now deleted** (§4) |
| 4 | `asSlice(off,size,align)` predicate and messages | wrong rule, wrong two messages, wrong order (§5) |
| 5 | `asSlice(off, MemoryLayout)` ignored the layout's alignment | fixed (§5) |
| 6 | `heapBase()` on a **read-only** segment handed out the writable array | fixed (§6) |
| 7 | heap `get`/`set` alignment gate was one conjunct too strict | fixed (§7) |
| 8 | `sequenceLayout`/`structLayout` overflow threw the wrong exception class | fixed (§8) |
| 9 | `SequenceLayout.elementCount()` is **not** derivable from `byteSize` | fixed (§8) |

---

## 1. The answer to the merge's UNSETTLED item: `maxByteAlignment()`

**MEASURED.** For a heap segment the answer is the **backing array's element
alignment**, capped by the lowest set bit of the segment's offset within that
array. Offset 0 answers the element alignment outright.

```
maxByteAlignment(heap) = byteOffset == 0
                       ? elementAlignment
                       : min(elementAlignment, lowestOneBit(byteOffset))
```

Equivalently, and this is what the JDK computes:
`address() == 0 ? maxAlignMask : Long.lowestOneBit(address() | maxAlignMask)`.

Transcribed, `FfmProbe` §A and `FfmProbe2` §P1 (every offset 0..=16):

| receiver | 0 | 1 | 2 | 3 | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|---|---|
| `byte[32]` | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| `short[16]` | 2 | 1 | 2 | 1 | 2 | 2 | 2 | 2 |
| `char[16]` | 2 | 1 | 2 | 1 | 2 | 2 | 2 | 2 |
| `int[8]` | 4 | 1 | 2 | 1 | 4 | 4 | 4 | 4 |
| `float[8]` | 4 | 1 | 2 | 1 | 4 | 4 | 4 | 4 |
| `long[4]` | 8 | 1 | 2 | 1 | 4 | 8 | 4 | 8 |
| `double[4]` | 8 | 1 | 2 | 1 | 4 | 8 | 4 | 8 |

and the zero-length rows, which matter because they show the answer comes from
the *type*, not from any byte:

```
ofArray(byte[0]).maxByteAlignment() = 1
ofArray(long[0]).maxByteAlignment() = 8
ofArray(byte[16]).asReadOnly().maxByteAlignment() = 1
ofBuffer(ByteBuffer.allocate(16)).maxByteAlignment() = 1
ofBuffer(IntBuffer.allocate(4)).maxByteAlignment() = 4
```

**The merge's guess was wrong on both arms.** It answered `addr == 0 ? 8 :
addr & -addr` over `pe_segment_base_address`. At offset 0 that is **8 for every
heap segment**, where the oracle says 1 for `byte[]`, 2 for `short[]`/`char[]`,
4 for `int[]`/`float[]` and 8 only for `long[]`/`double[]`. The merge note
reasoned that reading the raw `segment_address` "would have claimed 8-byte
alignment for every heap slice" — correct diagnosis, and then it shipped
exactly that answer through the other reader.

**And the native arm was wrong too.** A base of **0** answers
**4611686018427387904 = 2^62**, not 8:

```
MemorySegment.NULL.maxByteAlignment()  = 4611686018427387904
ofAddress(0).maxByteAlignment()        = 4611686018427387904
ofAddress(16).maxByteAlignment()       = 16
ofAddress(12).maxByteAlignment()       = 4
ofAddress(1).maxByteAlignment()        = 1
MemorySegment.NULL.asSlice(0, 0, 4096) = a segment       <- 8 would have refused
```

### 1.1 The rule is ONE predicate, and it is the same predicate everywhere

**MEASURED, as a sweep rather than as an assertion.** `FfmProbe2` §P2/§P3/§P4
cross every offset 0..=16 against alignments 1/2/4/8/16 on `byte[32]`,
`short[16]`, `int[8]`, `long[4]` and a native `allocate(32,16)`, and compare
each call's success against `seg.asSlice(off).maxByteAlignment()` **as HotSpot
itself computes it**. Nine sweeps, **zero mismatches**:

> `asSlice(off, size, align)` succeeds iff `align` is a positive power of two
> and `align <= seg.asSlice(off).maxByteAlignment()`.
>
> `get(layout, off)` succeeds iff
> `layout.byteAlignment() <= seg.asSlice(off).maxByteAlignment()`.

There is now one function, `pe_segment_max_byte_alignment`, and
`maxByteAlignment()`, both alignment-checked `asSlice` arities,
`spliterator`/`elements`, `toArray` and the heap `get`/`set` gate all go
through it.

---

## 2. The three untested callers: what the oracle says they do

**MEASURED**, `FfmProbe` §G and `FfmProbe3` §M1.

| call | oracle |
|---|---|
| `ofArray(byte[]{0..15}).toArray(JAVA_BYTE)` | `[0, 1, ..., 15]` |
| `ofArray(byte[16]).asSlice(3,4).toArray(JAVA_BYTE)` | `[3, 4, 5, 6]` |
| `ofArray(byte[16]).asSlice(3,4).toArray(JAVA_INT_UNALIGNED)` | `[100992003]` |
| `ofArray(int[]{10..80}).toArray(JAVA_INT)` | `[10, 20, ..., 80]` |
| `ofArray(int[8]).toArray(JAVA_BYTE).length` | 32 |
| `ofArray(byte[16]).toArray(JAVA_INT)` | IAE `Source segment incompatible with alignment constraints` |
| `ofArray(byte[15]).toArray(JAVA_INT)` | ISE `Segment size is not a multiple of 4. Size: 15` |
| `ofArray(byte[16]).toArray((OfByte) null)` | NPE `Cannot invoke "java.lang.foreign.ValueLayout.byteSize()" because "elemLayout" is null` |
| `ofArray(byte[16]).elements(JAVA_BYTE).count()` | 16 |
| `ofArray(byte[16]).elements(JAVA_INT)` | IAE `Incompatible alignment constraints` |
| `ofArray(byte[16]).elements(JAVA_INT_UNALIGNED).count()` | 4 |
| `ofArray(byte[16]).elements(paddingLayout(4)).count()` | 4 |
| `ofArray(byte[16]).elements(structLayout(JAVA_BYTE,JAVA_BYTE)).count()` | 8 |
| `ofArray(byte[15]).elements(JAVA_INT_UNALIGNED)` | IAE `Segment size is not a multiple of layout size` |
| `ofArray(byte[16]).elements(structLayout())` | IAE `Element layout size cannot be zero` |
| `ofArray(int[8]).elements(JAVA_INT)` element `.isNative()` | `false` |
| ... element `.heapBase().isPresent()` | `true` |
| ... element #1 `.address()` | 4 |
| `ofArray(int[8]).asReadOnly().elements(JAVA_INT)` element `.isReadOnly()` | `true` |
| `ofArray(int[8]).asSlice(2).elements(JAVA_INT)` | IAE `Incompatible alignment constraints` |
| `ofArray(int[8]).asSlice(4).elements(JAVA_INT).count()` | 7 |
| `ofArray(byte[16]).spliterator(JAVA_BYTE).estimateSize()` | 16 |
| `ofArray(byte[16]).spliterator(JAVA_BYTE).characteristics()` | 17744 |
| `ofArray(byte[16]).spliterator(JAVA_INT)` | IAE `Incompatible alignment constraints` |
| `ofArray(int[8]).spliterator(JAVA_INT).estimateSize()` | 8 |
| `ofArray(byte[16]).spliterator(null)` / `elements(null)` | NPE, null message |

Two orderings, both measured and both counter-intuitive:

* `elements`/`spliterator` check **alignment before size**:
  `ofArray(byte[15]).elements(JAVA_INT)` is the alignment message, not the
  size-multiple one.
* `toArray` checks **size before alignment**: the same doubly-bad receiver
  reports the size, because `AbstractMemorySegmentImpl.toArray` runs
  `checkArraySize` first and only then hands the segment to
  `MemorySegment.copy`.

---

## 3. `toArray` on a heap receiver returned ZEROS

**SOURCE-VERIFIED defect, now fixed.** `pe_segment_to_array` read the segment
through `panama_libffi::segment_address(...) as *const u8`, and that answers
**0** for every heap carrier by design (F27: a length is not an address). The
very next lines were

```rust
let arr = ctx.new_array(kind, count as usize);
if base.is_null() {
    return Ok(Some(Value::Object(Some(arr))));   // <- a correctly-sized array of zeros
}
```

so `MemorySegment.ofArray(new byte[]{1,2,3}).toArray(JAVA_BYTE)` answered
`[0, 0, 0]` where the oracle answers `[1, 2, 3]`. Not a refusal — a **silent
wrong answer**, on the one method whose entire job is to hand the bytes back.

This is the largest of the three untested callers precisely because it does not
go through `pe_segment_slice`: the merge note lists `toArray` alongside
`spliterator` and `elements` as newly reaching the heap arm, but `toArray` has
no slice in it at all, so the heap arm never reached it and nothing said so.

Fixed by giving it the same `heap_segment_view` + `heap_segment_read` arm that
`pe_segment_get_impl` uses — one reader, so `toArray` and a loop of `get`s
cannot disagree about stride or byte order — plus the measured alignment gate
in the measured position.

### 3.1 Why these bodies are reached at all — the registration evidence

Per HANDOFF §5, the question is not "does it compile" but "does the body I
edited win". What I could establish **without a binary**, and what I could not:

**Established (SOURCE-VERIFIED).**

1. **Each triple is registered exactly once in the whole repo.** Repo-wide
   grep: `"maxByteAlignment"` -> one hit (`panama.rs`); `"heapBase"` -> one hit
   (`panama.rs`); `"toArray"` on `java/lang/foreign/MemorySegment` -> one hit;
   `"spliterator"`/`"elements"` on that class -> one each. There is no second
   registrar to shadow them, so F21-1's failure mode does not apply here.
2. **Even if one appeared, `panama.rs` would win.** `lib.rs:10135` calls
   `register_p67_foreign_memory`, `lib.rs:10139` calls
   `register_pe_memory_segment` — in that order, and `register()` is
   last-write-wins. The synthetic path (`register_pe_panama`, `lib.rs:24348`)
   also ends at `register_pe_memory_segment` (`panama.rs:378`). No registrar
   for these classes runs after it in either mode.
3. **`asSlice` is force-routed; the other five are not.**
   `vm/src/runtime/interpreter/native_override.rs:4027` routes
   `java/lang/foreign/MemorySegment` to natives for exactly
   `byteSize | address | copy | get | set | getAtIndex | setAtIndex | asSlice |
   isNative | isMapped | isReadOnly | scope | ofArray`.
   `maxByteAlignment`, `heapBase`, `toArray`, `elements`, `spliterator`,
   `asReadOnly` and `asByteBuffer` are **absent** from that list.

**What (3) means, and it is the reachability chain that matters here.** On a
*real* `HeapMemorySegmentImpl$OfByte` in `--jdk-only` mode, `toArray` runs the
JDK's own bytecode and my body never sees it. But `asSlice` **is** force-routed,
and `pe_segment_slice` mints a carrier whose class is the *interface*
`java/lang/foreign/MemorySegment`. Every subsequent call on that slice —
`toArray`, `elements`, `spliterator`, `maxByteAlignment`, `heapBase` — resolves
to an interface method with no Code and therefore to **my** registration. So:

> These bodies serve every CratonVM-minted carrier, and since the merge, that
> set includes **every slice of a real heap segment**. That is the blast radius
> the merge note pointed at, and it is why `toArray` returning zeros mattered.

**NOT established.** I could not run
`--dump-native-registry` and therefore have **no `owns_slot`/`invocations`
evidence for any row**. Everything in (1)–(3) is read off source. The
orchestrator must confirm with the dump; see §11.

---

## 4. The deleted group-layout family — confirmed, and one more went with it

**SOURCE-VERIFIED.** Repo-wide grep for `pe_struct_layout`, `pe_union_layout`,
`pe_sequence_layout`, `pe_memory_layout_path_target`,
`pe_memory_layout_var_handle`, `pe_layout_name_value`: **six hits, all inside
comments**, all in `panama.rs`. Nothing calls them; nothing needs them.

The merge said `pe_memory_layout_width` was "the ONLY survivor". Once
`asSlice(long, MemoryLayout)` was corrected to take the layout's **alignment**
as well as its size (§5), that survivor had **zero callers** — a `dead_code`
warning waiting for the next build. It is deleted, with a tombstone. Its reason
for existing was to reconcile two carrier encodings, and there is only one left
to reconcile: `pe_make_layout`, the sole minter of the old `[0]=Int(kind)`
shape, is `#[cfg(test)]`.

**The carrier encoding is used consistently**, with one correction:
`p67_layout_size_align` (slots 0 and 1) and `p67_sequence_element_layout`
(slot 2) are the only readers of the prefix, and every factory writes
`[0]=byteSize, [1]=byteAlignment, [2]=payload, [3]=name`. §8.2 adds a
sequence-only slot 4 and explains why that is not a return of the six-slot
carrier F16 removed.

---

## 5. `asSlice` — the rule, both messages, and the order

**MEASURED**, `FfmProbe` §C, `FfmProbe2` §P5, `FfmProbe3` §M3.

| call | oracle |
|---|---|
| `byte[16].asSlice(17)` / `asSlice(-1)` / `asSlice(4,13)` / `asSlice(4,-1)` | `IndexOutOfBoundsException: Out of bound access on segment MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 16 }; new offset = 17; new length = 0` |
| `byte[16].asSlice(16)` / `asSlice(16,0)` / `asSlice(0,0)` | a zero-length segment |
| `byte[16].asSlice(4,4,3)` | IAE `Invalid alignment constraint : 3` |
| `byte[16].asSlice(4,4,0)` / `asSlice(4,4,-4)` | IAE `Invalid alignment constraint : 0` / `: -4` |
| `byte[16].asSlice(4,4,4)` / `asSlice(0,8,8)` | IAE `Target offset incompatible with alignment constraints` |
| `byte[16].asSlice(4,4,1)` | OK |
| `byte[16].asSlice(20,4,3)` | **IndexOutOfBoundsException** — bounds beat both |
| `int[8].asSlice(0,4,4)` / `asSlice(4,4,4)` | OK |
| `int[8].asSlice(2,4,4)` / `asSlice(0,8,8)` | IAE, alignment |
| `byte[16].asSlice(0, JAVA_INT)` | IAE, alignment |
| `byte[16].asSlice(0, JAVA_INT_UNALIGNED)` | 4-byte slice |
| `byte[16].asSlice(0, sequenceLayout(2, JAVA_INT))` | IAE, alignment |
| `byte[16].asSlice(0, paddingLayout(4))` | 4-byte slice |
| `int[8].asSlice(2, JAVA_INT)` | IAE, alignment |
| `byte[16].asSlice(0, (MemoryLayout) null)` | NPE |

Four things were wrong and all four are fixed:

1. **The exception class for a bad slice.** CratonVM raised
   `IllegalStateException: slice offset X + size Y exceeds segment size Z`. The
   oracle raises `IndexOutOfBoundsException`. A caller writing
   `catch (IndexOutOfBoundsException)` — which is what the heap `get`/`set`
   path in this same file already answers — could not catch it. (The bracketed
   receiver text embeds HotSpot's `toString()` with an identity hash we cannot
   reproduce; the class and the two trailing clauses are exact.)
2. **`Invalid alignment constraint : 3`** — note the space before the colon.
   CratonVM wrote `Invalid alignment constraint: {align}`.
3. **`Target offset incompatible with alignment constraints`** — no numbers at
   all. CratonVM wrote `Target offset {offset} incompatible with alignment
   {align}`, a different string on every row.
4. **The predicate.** CratonVM tested `(base + offset) % align != 0`. On a heap
   segment the base is the offset within the array and 0 at the start, so
   `byte[16].asSlice(0, 8, 8)` was **accepted** where the oracle refuses it.
   It is now `align <= maxByteAlignment(base + offset)`.

And `asSlice(long, MemoryLayout)` applied **no** alignment constraint at all,
because it only ever asked for the layout's width.

---

## 6. `heapBase()` on a read-only segment — a capability, handed back

**MEASURED**, `FfmProbe` B12 and `FfmProbe3` §M4:

| call | oracle |
|---|---|
| `ofArray(byte[16]).heapBase().isPresent()` | `true` |
| `ofArray(byte[16]).asReadOnly().heapBase().isPresent()` | **`false`** |
| `asReadOnly().asSlice(3,4).heapBase().isPresent()` | `false` |
| `asReadOnly().elements(JAVA_BYTE)` first `.heapBase().isPresent()` | `false` |
| `ofBuffer(ByteBuffer.allocate(8).asReadOnlyBuffer()).heapBase().isPresent()` | `false` |
| native `allocate(8).asReadOnly().heapBase().isPresent()` | `false` |
| `asReadOnly().toArray(JAVA_BYTE).length` | 16 — reads still work |

CratonVM returned the array regardless. The array `heapBase()` hands back is
the segment's own storage and is fully writable through plain array stores, so
returning it from a read-only view **gives back exactly the capability
`asReadOnly()` was called to remove**. This is F26-1's "a copying slice is a
wrong capability" and F21-1's "read-only is contagious", one call further on:
the contagion was implemented correctly through `asSlice`, and then the
backing array walked out of the front door.

The rule now lives in a free function `pe_segment_heap_base` rather than in the
registration closure, so it has a test — the same reason F35 lifted
`pe_segment_as_slice` out of its closure.

---

## 7. The heap `get`/`set` gate had one conjunct too many

**MEASURED**, `FfmProbe3` §M1. The gate read

```rust
let max_align = if view.start == 0 { elem } else { min(elem, lowbit(view.start)) };
if align > max_align || (view.start + offset) % align != 0 { refuse }
```

which is `align <= elem && align <= lowbit(start) && align <= lowbit(start+offset)`.
The oracle's rule is the first and third conjuncts only. The middle one makes
CratonVM **stricter than HotSpot** whenever a slice starts at a worse alignment
than an offset inside it reaches:

| call | oracle | CratonVM before |
|---|---|---|
| `ofArray(int[8]).asSlice(2).maxByteAlignment()` | 2 | (n/a) |
| `ofArray(int[8]).asSlice(2).get(JAVA_INT, 2)` | reads — absolute 4 | **refused** |
| `ofArray(int[8]).asSlice(2).get(JAVA_INT, 0)` | IAE — absolute 2 | refused |
| `ofArray(long[4]).asSlice(4).get(JAVA_LONG, 4)` | reads — absolute 8 | **refused** |
| `ofArray(int[8]).asSlice(2).asSlice(2).maxByteAlignment()` | 4 | (n/a) |

The existing test `heap_alignment_is_enforced_the_way_the_oracle_enforces_it`
carries a "MUTATION" note claiming the rule has two halves that must both be
kept. It has two halves, but they are the element width and the low bit of the
**absolute** offset — not the slice start. All five of that test's rows stay
green under the corrected predicate (each is at `start == 0`, where the two
spellings coincide), which is exactly why it did not catch this.

---

## 8. The layout factories the merge just wrote

Four of the merge's own new rows were PREDICTED from a reading of the JDK's
source rather than measured, and four are wrong.

### 8.1 Overflow is `IllegalArgumentException`, not `ArithmeticException`

**MEASURED**, `FfmProbe4` §N and `P6`:

```
sequenceLayout(Long.MAX_VALUE, JAVA_INT)    -> IllegalArgumentException: Layout size exceeds Long.MAX_VALUE
sequenceLayout(Long.MAX_VALUE/2+1, SHORT)   -> the same
sequenceLayout(Long.MAX_VALUE, JAVA_BYTE)   -> 9223372036854775807   (no overflow, no throw)
structLayout(big, JAVA_BYTE)                -> IllegalArgumentException: Layout size exceeds Long.MAX_VALUE
structLayout(big, big)                      -> the same
unionLayout(big, JAVA_BYTE)                 -> 9223372036854775807   (max, never sums)
```
(`big = sequenceLayout(Long.MAX_VALUE, JAVA_BYTE)`.)

The merge wrote, in both factories, "`Math.multiplyExact` … so an overflow is
an ArithmeticException, not a saturated size" and "`Math.addExact` in the JDK;
an overflow there is an ArithmeticException, not a silent wrap." The JDK does
call those methods — and does not let the `ArithmeticException` out. A caller
catching `IllegalArgumentException`, which is what every other refusal in these
two factories throws, would not have caught it. Both are now IAE with the
transcribed message.

### 8.2 `elementCount()` is NOT derivable from `byteSize`

**MEASURED**, `FfmProbe4` N6–N12, `P5` O3–O5, `P7` R1:

```
sequenceLayout(3, structLayout()).byteSize()      = 0
sequenceLayout(3, structLayout()).elementCount()  = 3      <- not 0
sequenceLayout(3, structLayout()).toString()      = [3:[]]
sequenceLayout(2, sequenceLayout(0, JAVA_INT)).elementCount() = 2
sequenceLayout(3, unionLayout()).byteSize()       = 0
arena.allocate(sequenceLayout(3, structLayout())).byteSize() = 0
```

The merge's carrier note says: "The count is not stored because it is
derivable, and a stored copy is one more thing that can disagree with
`byteSize`: `elementCount()` below divides the total by the element size."
An element layout may have byteSize **zero** — `structLayout()` and
`unionLayout()` both do — and then the total is 0 for every count and the
division answers 0 for all of them. (It is guarded against a divide-by-zero, so
this was a quiet wrong number and not a panic.)

The count is now stored at **slot 4** of the sequence carrier, and
`elementCount()` and `p67_layout_render` prefer it, falling back to the
division only for a four-slot carrier minted elsewhere. **This is not the
six-slot carrier F16 removed**: that one put the *element* at slot 4, where the
`byteOffset` and `varHandle` walks expect it at slot 2. The shared prefix
`[0]=byteSize, [1]=byteAlignment, [2]=payload, [3]=name` is untouched, and slot
4 is read by `elementCount()` and the renderer alone.

### 8.3 The element-layout check the factory was missing

**MEASURED**, `FfmProbe4` N30–N36:

```
sequenceLayout(2, structLayout(JAVA_INT, JAVA_BYTE)) -> IAE: Element layout size is not multiple of alignment
sequenceLayout(1, same)                              -> the same
sequenceLayout(0, same)                              -> the same   <- even at count 0
sequenceLayout(2, JAVA_INT.withByteAlignment(8))     -> the same
sequenceLayout(2, unionLayout(structLayout(JAVA_INT, JAVA_BYTE))) -> the same
sequenceLayout(2, structLayout(JAVA_INT, JAVA_BYTE, paddingLayout(3))) -> 16
sequenceLayout(2, unionLayout(JAVA_INT, JAVA_BYTE))  -> 8
sequenceLayout(2, paddingLayout(3))                  -> 6
```

CratonVM accepted the first five and answered a 10-byte sequence with alignment
4 whose second element starts at byte 5. `pe_segment_spliterator` already
carried this exact check and this exact message for a *segment's* element
layout; the oracle puts it at the factory too, before the count is multiplied
in — which is why count 0 refuses.

### 8.4 A null member is a NullPointerException, not a member to skip

**MEASURED**, `FfmProbe4` N20–N24, `P5` O2:

```
structLayout((MemoryLayout) null)  -> NullPointerException (null message)
structLayout(JAVA_INT, null)       -> NullPointerException
unionLayout(JAVA_INT, null)        -> NullPointerException
sequenceLayout(4, null)            -> NullPointerException
sequenceLayout(-1, null)           -> IAE: The provided elementCount is negative: -1
```

All three factories used `continue` / a `None` default, so
`structLayout(JAVA_LONG, null)` answered `j8`, byteSize 8, with nothing to say
a member had gone missing. The last row fixes the ORDER: the negative-count
check wins even over a null element, so it stays first.

### 8.5 What the merge got RIGHT, confirmed row by row

Worth stating, because it is most of the surface (`FfmProbe` §D, `FfmProbe2`
§P7):

```
structLayout(JAVA_BYTE, JAVA_INT)      -> IAE: Invalid alignment constraint for member layout: i4
structLayout(JAVA_BYTE, JAVA_SHORT)    -> ... : s2
structLayout(JAVA_INT,  JAVA_LONG)     -> ... : j8
structLayout(JAVA_BYTE, ADDRESS)       -> ... : a8
structLayout(JAVA_BYTE, JAVA_INT.withName("a")) -> ... : i4(a)
structLayout(JAVA_BYTE, sequenceLayout(2, JAVA_INT)) -> ... : [2:i4]
structLayout(JAVA_BYTE, structLayout(JAVA_INT))      -> ... : [i4]
structLayout(pad(1), JAVA_INT)         -> ... : i4
structLayout(JAVA_INT, pad(1), JAVA_INT) -> ... : i4
structLayout(JAVA_LONG, JAVA_INT)      -> 12, align 8         (NOT 16)
structLayout(JAVA_INT,  JAVA_BYTE)     -> 5,  align 4         (no trailing pad)
structLayout(JAVA_BYTE, pad(3), JAVA_INT) -> 8, align 4
structLayout(JAVA_BYTE, JAVA_BYTE, JAVA_BYTE, JAVA_BYTE, JAVA_INT) -> 8
structLayout(JAVA_INT_UNALIGNED, JAVA_LONG_UNALIGNED) -> 12, align 1
structLayout()                         -> 0,  align 1
unionLayout(JAVA_INT, JAVA_LONG)       -> 8,  align 8
unionLayout(JAVA_BYTE, JAVA_INT)       -> 4,  align 4
unionLayout(structLayout(JAVA_INT, JAVA_BYTE)) -> 5, align 4  (no rounding)
unionLayout(paddingLayout(3))          -> 3,  align 1
unionLayout()                          -> 0,  align 1
paddingLayout(4)                       -> 4,  align 1
paddingLayout(0) / paddingLayout(-1)   -> IAE: Invalid byte size: 0 / -1
sequenceLayout(-1, JAVA_INT)           -> IAE: The provided elementCount is negative: -1
JAVA_INT.withByteAlignment(3)          -> IAE: Invalid alignment: 3
```

F16-1's headline claim — **the JDK never auto-pads a struct, and
`structLayout(JAVA_LONG, JAVA_INT)` is 12** — is CONFIRMED MEASURED, as is the
merge's `unionLayout` "no rounding" note (5, not 8).

---

## 9. Measured surface this lane did NOT act on

Recorded so the next lane does not have to re-measure. All **MEASURED**, none
fixed.

### 9.1 A closed arena permits more than CratonVM does

```
closed.byteSize()          = 16          closed.get(JAVA_BYTE,0)   = ISE: Already closed
closed.address() != 0      = true        closed.fill((byte)1)      = ISE: Already closed
closed.isNative()          = true        closed.toArray(JAVA_BYTE) = ISE: Already closed
closed.asSlice(0,4)        = works       closed.mismatch(...)      = ISE: Already closed
closed.asByteBuffer()      = 16          closed arena.close()      = ISE: Already closed
closed.maxByteAlignment()  = 16          closed arena.allocate(8)  = ISE: Already closed
closed.elements(JAVA_BYTE).count() = 16  closed scope.isAlive()    = false
closed.isAccessibleBy(currentThread()) = true
```

`byteSize`, `address`, `isNative`, `asSlice`, `asByteBuffer`,
`maxByteAlignment`, `isAccessibleBy` and — surprisingly —
`elements(...).count()` all work **after close**, because none of them touches
memory. `pe_segment_spliterator` calls `pe_segment_check_scope` first, so
CratonVM refuses `elements` on a closed segment where HotSpot returns 16.
Not fixed: removing a scope check is the kind of change that wants a running VM
behind it.

### 9.2 Wrong-thread access (`MemorySession.checkValidState`, W7-89)

Confined arena, accessed from another thread:

```
get(JAVA_BYTE,0)  -> WrongThreadException: Attempted access outside owning thread
arena.allocate(8) -> WrongThreadException: Attempted access outside owning thread
arena.close()     -> WrongThreadException: Attempted access outside owning thread
byteSize()        -> 16          <- allowed
asSlice(0,4)      -> works       <- allowed
isAccessibleBy(currentThread()) -> false, no throw
```

Shared arena from another thread: `get` reads, `isAccessibleBy` is `true`,
`close()` succeeds, and the segment then reports
`IllegalStateException: Already closed`. A heap segment's scope never closes
and is accessible from every thread.

`WrongThreadException` with the message `Attempted access outside owning
thread` is the transcription W7-89 needs. CratonVM's session model was not
audited by this lane.

### 9.3 The heap `get`/`set` refusal message is not the oracle's

```
HotSpot:  Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment
          MemorySegment{ kind: heap, heapBase: [B@7c1503a3, address: 0x0, byteSize: 16 }
CratonVM: Target offset 0 is incompatible with alignment constraint 4 for segment
          MemorySegment{ kind: heap, address: 0x0, byteSize: 16 }
```

Two differences: the `(of i4)` layout rendering, and `heapBase: [B@…` in the
receiver text. `p67_layout_render` can produce `i4` today (§8.5 shows it
matching the oracle on nine spellings), so the first half is a small change;
the second needs an identity hash CratonVM cannot reproduce. Left alone
deliberately — this lane changed exception CLASSES where they were wrong, and
did not start rewriting message texts that only differ in decoration.

### 9.4 The native `get`/`set` path still does not check alignment

The oracle enforces the same rule on a native segment (`FfmProbe2` §P4, native
sweep, zero mismatches), and `pe_segment_max_byte_alignment` can now compute it
for one. It is still **not** applied on the raw-address path, and deliberately:
CratonVM's own allocator's alignment behaviour has never been measured, so
turning the check on could refuse calls that work today (netty's FFM allocator
is on that path) for a reason no measurement supports. This is the one place
where a correct-looking change is a bigger risk than the divergence.

### 9.5 Other measured rows worth having

```
ofArray(int[8]).asByteBuffer()  -> UnsupportedOperationException: Not an address to an heap-allocated byte array
ofArray(byte[16]).asByteBuffer().isDirect() -> false
ofArray(byte[8]).reinterpret(4) -> UnsupportedOperationException: Not a native segment
ofArray(byte[16]).isAccessibleBy(null) -> NullPointerException
Arena.global().close() / Arena.ofAuto().close() -> UnsupportedOperationException: Attempted to close a non-closeable session
arena.allocate(-1)   -> IAE: The provided allocation size is negative: -1
arena.allocate(8, 3) -> IAE: Invalid alignment constraint : 3
arena.allocateFrom("abcd") -> byteSize 5, bytes [97, 98, 99, 100, 0]
arena.allocateFrom("")     -> byteSize 1
arena.allocateFrom((String) null) -> NullPointerException
arena.allocateFrom(JAVA_INT, 1, 2, 3) -> byteSize 12
setString / copy / fill into a read-only segment -> IAE: Attempt to write a read-only segment
mismatch(null) -> NullPointerException;  mismatch(equal) -> -1
byteOffset(groupElement("zz")) -> IAE: Bad layout path: cannot resolve 'zz' in layout [i4(a)i4(b)]
seq.byteOffset(sequenceElement(4)) -> IAE: Bad layout path: sequence index out of bounds; index: 4, elementCount is 4 for layout [4:i4]
varHandle set on a read-only segment -> IAE: Attempt to write a read-only segment
sequenceLayout(4,JAVA_INT).varHandle(sequenceElement()) on a byte[16] heap segment
   -> IAE: Target offset 0 is incompatible with alignment constraint 4 (of [4:i4]) ...
```

The `getClass()` rows, which are what makes CratonVM's fabrication visible:

```
ofArray(byte[16]).getClass()  = jdk.internal.foreign.HeapMemorySegmentImpl$OfByte
ofArray(byte[16]).asSlice(3,4).getClass() = jdk.internal.foreign.HeapMemorySegmentImpl$OfByte
ValueLayout.JAVA_INT.getClass()           = jdk.internal.foreign.layout.ValueLayouts$OfIntImpl
structLayout(JAVA_INT).getClass()         = jdk.internal.foreign.layout.StructLayoutImpl
sequenceLayout(2,JAVA_INT).getClass()     = jdk.internal.foreign.layout.SequenceLayoutImpl
paddingLayout(4).getClass()               = jdk.internal.foreign.layout.PaddingLayoutImpl
unionLayout(JAVA_INT).getClass()          = jdk.internal.foreign.layout.UnionLayoutImpl
```

Note row 2: HotSpot's slice of a heap segment is still a
`HeapMemorySegmentImpl$OfByte`. CratonVM's is an instance of the *interface*.
That is the fabrication that makes §3.1's reachability chain work, and also the
reason `SegmentAllocator.allocateFrom` once died in a `ClassCastException`
inside the JDK.

---

## 10. NOMINATIONS — everything outside this lane's two files

**NOM-1 (the one that matters most).**
`native-builtins/src/test_utils.rs`, `mock_field_slot` at **line 306**.
Its whole chain — including `cratonvm_classloading::synthetic_stub_field_model`
— has **no entry for `jdk/internal/foreign/HeapMemorySegmentImpl$Of*`**, and
`grep '"base"' native-builtins/src/test_utils.rs` is empty. An unresolved name
is a silent no-op on `set_field_by_name` and `Value::Int(0)` on
`get_field_by_name`, and `heap_seg_field` only falls through to the class-side
resolver when the answer is `Value::Object(None)`. Therefore
`make_real_heap_segment` (`panama.rs:8111`) builds a carrier for which
`heap_segment_view` answers **`None`**, and `pe_segment_get_impl` takes its
`unreadable_heap_segment` arm.

**PREDICTED consequence, and the orchestrator should check it first:** the F35
tests `heap_alignment_is_enforced_the_way_the_oracle_enforces_it` and
`heap_refusals_use_the_oracles_exception_classes` assert `.is_ok()` on that
path and are therefore **red, or green for the wrong reason**. This lane
sidestepped it by building every new test on the eight-slot H2 carrier, which
resolves by slot and is proven to work by
`of_array_covers_byte_short_and_char_and_the_carrier_aliases` (it asserts a
write reaching the caller's array).

Change: add to the `mock_field_slot` chain

```rust
fn mock_foreign_segment_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match class_name {
        // javap, 25.0.3+9-LTS: AbstractMemorySegmentImpl{length, readOnly, scope}
        // then HeapMemorySegmentImpl{offset, base}.
        Some(c) if c.starts_with("jdk/internal/foreign/HeapMemorySegmentImpl") => match name {
            "length" => Some(0),
            "readOnly" => Some(1),
            "scope" => Some(2),
            "offset" => Some(3),
            "base" => Some(4),
            _ => None,
        },
        _ => None,
    }
}
```

**NOM-2.** `native-builtins/src/panama_libffi.rs` **line 370**, the
`LAYOUT_SEQUENCE` arm of `layout_to_ffi_type`. Its banner "THE ELEMENT COUNT IS
NOT STORED" is now stale (§8.2) and its `count = total / elem_size` should
prefer slot 4 when `object_num_fields(layout) > 4`. It is not *wrong* today —
it refuses `elem_size == 0` before dividing — but it is the second copy of a
rule that now has an authoritative reader.

**NOM-3.** `native-builtins/src/panama_libffi.rs` **line 1484**, the test
`sequence_carrier_reports_its_total_not_its_alignment`, builds a four-slot
sequence carrier. Still valid as the fallback case; it should gain a five-slot
sibling so the stored count has a test on that side too.

**NOM-4 (a decision, not a bug).**
`vm/src/runtime/interpreter/native_override.rs` **line 4027**. The
`MemorySegment` force-route list omits `maxByteAlignment`, `heapBase`,
`toArray`, `elements`, `spliterator`, `asReadOnly`, `asByteBuffer`, `mismatch`,
`fill` and `isAccessibleBy`. On a *real* segment in `--jdk-only` mode the JDK's
own bytecode runs those, which is more correct than any body we could write —
so the omission may be deliberate. But the split is invisible from
`panama.rs`, and it is the reason the fixes in §1, §3 and §6 serve
CratonVM-minted carriers only. Whoever owns that file should either document
the split or close it; do not change it without the registry dump.

**NOM-5.** `native-builtins/src/panama_libffi.rs`, `read_layout_kind`'s
`_ => LAYOUT_LONG` default (referenced at lines 132, 162, 576, 1365). Already
nominated by the merge; restated because §8.2's slot-4 change makes the
sequence path one reader shorter and this is now the last reconciliation layer
between the two encodings.

---

## 11. What the orchestrator must check at build time

1. **`cargo test -p cratonvm-native-builtins panama`.** Seven new tests. If
   `heap_alignment_is_enforced_the_way_the_oracle_enforces_it` or
   `heap_refusals_use_the_oracles_exception_classes` is red, that is NOM-1 and
   it predates this lane — apply NOM-1 rather than reverting anything here.
2. **`--dump-native-registry`.** For each of
   `(java/lang/foreign/MemorySegment, maxByteAlignment, ()J)`,
   `(…, heapBase, ()Ljava/util/Optional;)`,
   `(…, toArray, (Ljava/lang/foreign/ValueLayout$OfByte;)[B)`,
   `(…, elements, (Ljava/lang/foreign/MemoryLayout;)Ljava/util/stream/Stream;)`,
   `(…, spliterator, (Ljava/lang/foreign/MemoryLayout;)Ljava/util/Spliterator;)`
   and the four `asSlice` descriptors: confirm `owns_slot=true`,
   `registered_by` is `register_pe_memory_segment`, `overwrote` is empty, and
   `invocations` is non-zero under the vector in §12. **I established
   last-write-wins by source order and by a repo-wide triple grep; I have no
   `invocations` evidence for any row.**
3. **`java/lang/foreign/SequenceLayout` is now allocated with five slots.**
   Confirm nothing else in the tree reads a sequence carrier positionally past
   slot 3 (repo grep says only `panama_libffi.rs:370` reads a sequence at all,
   and it reads slot 2).
4. `pe_memory_layout_width` is **deleted**. If anything fails to compile
   naming it, that is a caller this lane's grep missed.
5. Both files: zero CR bytes (`tr -cd '\r' < f | wc -c` == 0, verified), zero
   conflict markers (verified), `rustfmt --edition 2021 --check` produces
   **no new** diffs — `panama.rs` has 51 pre-existing formatting diffs and
   `foreign_ffm.rs` had 18, now 17, and none of them is in code this lane
   wrote (verified by diffing the counts against the staged versions).

---

## 12. Regression vector

**Do not create this under `regression-suite/`** — this lane is forbidden to.
It is here so whoever owns the suite can lift it whole. Every expectation is
the transcribed oracle value, and this exact source was **run on HotSpot
25.0.3+9-LTS: 86 checks, 86 green, zero FAIL lines.** So a failure on CratonVM
is a VM divergence and not a bad expectation. Labels are ASCII (HANDOFF §7);
the two rows whose oracle message embeds a receiver `toString()` with an
identity hash compare the exception CLASS only, via `exClass`.

```java
import java.lang.foreign.*;
import java.util.*;

/** RJdkFfmSegment - G6-1. ASCII labels only (HANDOFF section 7). */
public class RJdkFfmSegment {
    static int checks = 0;
    static void eq(String label, Object actual, Object expected) {
        checks++;
        String a = str(actual), e = str(expected);
        System.out.println((a.equals(e) ? "ok   " : "FAIL ") + label + " = " + a
                + (a.equals(e) ? "" : "   (expected " + e + ")"));
    }
    static String str(Object o) {
        if (o instanceof byte[] b) return Arrays.toString(b);
        if (o instanceof int[] i) return Arrays.toString(i);
        return String.valueOf(o);
    }
    static Object attempt(java.util.function.Supplier<Object> s) {
        try { return s.get(); }
        catch (Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    }
    /** The exception CLASS alone, for rows whose message embeds a receiver
     *  toString() with an identity hash no VM can be asked to reproduce. */
    static String exClass(java.util.function.Supplier<Object> s) {
        try { s.get(); return "no exception"; }
        catch (Throwable t) { return t.getClass().getName(); }
    }

    public static void main(String[] args) {
        // --- maxByteAlignment is the element type, not 8 ---
        eq("A1 byte[16] maxAlign",   MemorySegment.ofArray(new byte[16]).maxByteAlignment(),   1L);
        eq("A2 short[8] maxAlign",   MemorySegment.ofArray(new short[8]).maxByteAlignment(),   2L);
        eq("A3 char[8] maxAlign",    MemorySegment.ofArray(new char[8]).maxByteAlignment(),    2L);
        eq("A4 int[8] maxAlign",     MemorySegment.ofArray(new int[8]).maxByteAlignment(),     4L);
        eq("A5 long[8] maxAlign",    MemorySegment.ofArray(new long[8]).maxByteAlignment(),    8L);
        eq("A6 float[8] maxAlign",   MemorySegment.ofArray(new float[8]).maxByteAlignment(),   4L);
        eq("A7 double[8] maxAlign",  MemorySegment.ofArray(new double[8]).maxByteAlignment(),  8L);
        eq("A8 byte[0] maxAlign",    MemorySegment.ofArray(new byte[0]).maxByteAlignment(),    1L);
        eq("A9 long[0] maxAlign",    MemorySegment.ofArray(new long[0]).maxByteAlignment(),    8L);
        MemorySegment l4 = MemorySegment.ofArray(new long[4]);
        eq("A10 long[4] slice4 maxAlign",  l4.asSlice(4).maxByteAlignment(), 4L);
        eq("A11 long[4] slice8 maxAlign",  l4.asSlice(8).maxByteAlignment(), 8L);
        eq("A12 long[4] slice12 maxAlign", l4.asSlice(12).maxByteAlignment(), 4L);
        eq("A13 int[8] slice2 maxAlign",
                MemorySegment.ofArray(new int[8]).asSlice(2).maxByteAlignment(), 2L);
        eq("A14 NULL maxAlign", MemorySegment.NULL.maxByteAlignment(), 4611686018427387904L);
        eq("A15 ofAddress(12) maxAlign", MemorySegment.ofAddress(12).maxByteAlignment(), 4L);

        // --- toArray on a heap receiver reads the array, not address 0 ---
        MemorySegment b16 = MemorySegment.ofArray(
                new byte[]{0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15});
        MemorySegment i8 = MemorySegment.ofArray(new int[]{10,20,30,40,50,60,70,80});
        eq("B1 byte[16] toArray", b16.toArray(ValueLayout.JAVA_BYTE),
                new byte[]{0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15});
        eq("B2 slice(3,4) toArray", b16.asSlice(3,4).toArray(ValueLayout.JAVA_BYTE),
                new byte[]{3,4,5,6});
        eq("B3 int[8] toArray", i8.toArray(ValueLayout.JAVA_INT),
                new int[]{10,20,30,40,50,60,70,80});
        eq("B4 int[8] toArray(JAVA_BYTE).length", i8.toArray(ValueLayout.JAVA_BYTE).length, 32);
        eq("B5 byte[16] toArray(JAVA_INT_UNALIGNED)", b16.toArray(ValueLayout.JAVA_INT_UNALIGNED),
                new int[]{50462976, 117835012, 185207048, 252579084});
        eq("B6 byte[16] toArray(JAVA_INT)", attempt(() -> b16.toArray(ValueLayout.JAVA_INT)),
                "java.lang.IllegalArgumentException: Source segment incompatible with alignment constraints");
        eq("B7 byte[15] toArray(JAVA_INT)",
                attempt(() -> MemorySegment.ofArray(new byte[15]).toArray(ValueLayout.JAVA_INT)),
                "java.lang.IllegalStateException: Segment size is not a multiple of 4. Size: 15");
        eq("B8 int[8] toArray(JAVA_LONG)", attempt(() -> i8.toArray(ValueLayout.JAVA_LONG)),
                "java.lang.IllegalArgumentException: Source segment incompatible with alignment constraints");

        // --- elements / spliterator on a heap receiver ---
        eq("C1 byte[16] elements(JAVA_BYTE).count", b16.elements(ValueLayout.JAVA_BYTE).count(), 16L);
        eq("C2 byte[16] elements(JAVA_INT)", attempt(() -> b16.elements(ValueLayout.JAVA_INT).count()),
                "java.lang.IllegalArgumentException: Incompatible alignment constraints");
        eq("C3 byte[16] elements(JAVA_INT_UNALIGNED).count",
                b16.elements(ValueLayout.JAVA_INT_UNALIGNED).count(), 4L);
        eq("C4 int[8] elements(JAVA_INT).count", i8.elements(ValueLayout.JAVA_INT).count(), 8L);
        eq("C5 int[8] elements values",
                i8.elements(ValueLayout.JAVA_INT).map(s -> s.get(ValueLayout.JAVA_INT, 0)).toList()
                        .toString(), "[10, 20, 30, 40, 50, 60, 70, 80]");
        eq("C6 int[8] element isNative",
                i8.elements(ValueLayout.JAVA_INT).findFirst().get().isNative(), false);
        eq("C7 int[8] element heapBase present",
                i8.elements(ValueLayout.JAVA_INT).findFirst().get().heapBase().isPresent(), true);
        eq("C8 int[8] element 1 address",
                i8.elements(ValueLayout.JAVA_INT).skip(1).findFirst().get().address(), 4L);
        eq("C9 int[8] slice4 elements count",
                i8.asSlice(4).elements(ValueLayout.JAVA_INT).count(), 7L);
        eq("C10 byte[16] spliterator(JAVA_BYTE) estimateSize",
                b16.spliterator(ValueLayout.JAVA_BYTE).estimateSize(), 16L);
        eq("C11 int[8] spliterator(JAVA_INT) estimateSize",
                i8.spliterator(ValueLayout.JAVA_INT).estimateSize(), 8L);
        long[] sum = new long[1];
        i8.spliterator(ValueLayout.JAVA_INT)
          .forEachRemaining(s -> sum[0] += s.get(ValueLayout.JAVA_INT, 0));
        eq("C12 int[8] spliterator sum", sum[0], 360L);
        eq("C13 byte[15] elements(JAVA_INT_UNALIGNED)",
                attempt(() -> MemorySegment.ofArray(new byte[15])
                        .elements(ValueLayout.JAVA_INT_UNALIGNED).count()),
                "java.lang.IllegalArgumentException: Segment size is not a multiple of layout size");
        eq("C14 byte[16] elements(structLayout()) zero size",
                attempt(() -> b16.elements(MemoryLayout.structLayout()).count()),
                "java.lang.IllegalArgumentException: Element layout size cannot be zero");

        // --- read-only withholds the backing array, but still reads ---
        MemorySegment ro = b16.asReadOnly();
        eq("D1 writable heapBase present", b16.heapBase().isPresent(), true);
        eq("D2 readOnly heapBase present",  ro.heapBase().isPresent(), false);
        eq("D3 readOnly slice heapBase",    ro.asSlice(3,4).heapBase().isPresent(), false);
        eq("D4 readOnly slice isReadOnly",  ro.asSlice(3,4).isReadOnly(), true);
        eq("D5 readOnly element heapBase",
                ro.elements(ValueLayout.JAVA_BYTE).findFirst().get().heapBase().isPresent(), false);
        eq("D6 readOnly toArray length",    ro.toArray(ValueLayout.JAVA_BYTE).length, 16);
        eq("D7 readOnly set", attempt(() -> { ro.set(ValueLayout.JAVA_BYTE, 0, (byte)1); return "ok"; }),
                "java.lang.IllegalArgumentException: Attempt to write a read-only segment");

        // --- asSlice: rule, messages, order ---
        eq("E1 asSlice(4,4,3)",  attempt(() -> b16.asSlice(4,4,3).byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint : 3");
        eq("E2 asSlice(4,4,0)",  attempt(() -> b16.asSlice(4,4,0).byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint : 0");
        eq("E3 asSlice(0,8,8)",  attempt(() -> b16.asSlice(0,8,8).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E4 asSlice(4,4,1)",  b16.asSlice(4,4,1).byteSize(), 4L);
        eq("E5 int[8] asSlice(0,4,4)", i8.asSlice(0,4,4).byteSize(), 4L);
        eq("E6 int[8] asSlice(2,4,4)", attempt(() -> i8.asSlice(2,4,4).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E7 int[8] asSlice(0,8,8)", attempt(() -> i8.asSlice(0,8,8).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        // Bounds beat BOTH alignment checks: this offset is out of range and
        // the alignment is not a power of two, and the oracle reports bounds.
        eq("E8 asSlice(20,4,3) bounds first", exClass(() -> b16.asSlice(20,4,3)),
                "java.lang.IndexOutOfBoundsException");
        eq("E9 asSlice(17) class", exClass(() -> b16.asSlice(17)),
                "java.lang.IndexOutOfBoundsException");
        eq("E9b asSlice(4,-1) class", exClass(() -> b16.asSlice(4,-1)),
                "java.lang.IndexOutOfBoundsException");
        eq("E10 asSlice(0,JAVA_INT)", attempt(() -> b16.asSlice(0, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E11 asSlice(0,JAVA_INT_UNALIGNED)",
                b16.asSlice(0, ValueLayout.JAVA_INT_UNALIGNED).byteSize(), 4L);
        eq("E12 asSlice(0,paddingLayout(4))",
                b16.asSlice(0, MemoryLayout.paddingLayout(4)).byteSize(), 4L);
        eq("E13 asSlice(0,seq(2,JAVA_INT))",
                attempt(() -> b16.asSlice(0, MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_INT)).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E14 int[8] asSlice(4,JAVA_INT)", i8.asSlice(4, ValueLayout.JAVA_INT).byteSize(), 4L);

        // --- the slice's start does not disqualify a better offset inside it ---
        // i8 holds {10,20,...}; the slice starts at byte 2, so byte offset 2
        // inside it is absolute 4, which is element 1 == 20.
        eq("F1 int[8].asSlice(2).get(JAVA_INT,2)",
                i8.asSlice(2).get(ValueLayout.JAVA_INT, 2), 20);
        eq("F2 int[8].asSlice(2).get(JAVA_INT,0)",
                exClass(() -> i8.asSlice(2).get(ValueLayout.JAVA_INT, 0)),
                "java.lang.IllegalArgumentException");
        eq("F3 int[8].asSlice(2).maxByteAlignment", i8.asSlice(2).maxByteAlignment(), 2L);
        eq("F4 long[4].asSlice(4).get(JAVA_LONG,4)",
                MemorySegment.ofArray(new long[4]).asSlice(4).get(ValueLayout.JAVA_LONG, 4), 0L);
        eq("F5 long[4].asSlice(4).get(JAVA_LONG,0)",
                exClass(() -> MemorySegment.ofArray(new long[4]).asSlice(4)
                        .get(ValueLayout.JAVA_LONG, 0)),
                "java.lang.IllegalArgumentException");

        // --- layouts: the JDK never pads, and the refusals it makes instead ---
        eq("G1 struct(LONG,INT) byteSize",
                MemoryLayout.structLayout(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT).byteSize(), 12L);
        eq("G2 struct(INT,BYTE) byteSize",
                MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE).byteSize(), 5L);
        eq("G3 struct(BYTE,INT)",
                attempt(() -> MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint for member layout: i4");
        eq("G4 struct(BYTE,pad(3),INT) byteSize",
                MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, MemoryLayout.paddingLayout(3),
                        ValueLayout.JAVA_INT).byteSize(), 8L);
        eq("G5 struct() byteAlignment", MemoryLayout.structLayout().byteAlignment(), 1L);
        eq("G6 union(struct(INT,BYTE)) byteSize",
                MemoryLayout.unionLayout(MemoryLayout.structLayout(ValueLayout.JAVA_INT,
                        ValueLayout.JAVA_BYTE)).byteSize(), 5L);
        eq("G7 padding(0)", attempt(() -> MemoryLayout.paddingLayout(0).byteSize()),
                "java.lang.IllegalArgumentException: Invalid byte size: 0");
        eq("G8 padding(4) byteAlignment", MemoryLayout.paddingLayout(4).byteAlignment(), 1L);
        eq("G9 seq(-1,INT)", attempt(() -> MemoryLayout.sequenceLayout(-1, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: The provided elementCount is negative: -1");
        eq("G10 seq(MAX,INT)",
                attempt(() -> MemoryLayout.sequenceLayout(Long.MAX_VALUE, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: Layout size exceeds Long.MAX_VALUE");
        eq("G11 seq(MAX,BYTE) byteSize",
                MemoryLayout.sequenceLayout(Long.MAX_VALUE, ValueLayout.JAVA_BYTE).byteSize(),
                Long.MAX_VALUE);
        eq("G12 seq(2,struct(INT,BYTE))",
                attempt(() -> MemoryLayout.sequenceLayout(2,
                        MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE)).byteSize()),
                "java.lang.IllegalArgumentException: Element layout size is not multiple of alignment");
        eq("G13 seq(0,struct(INT,BYTE)) also refuses",
                attempt(() -> MemoryLayout.sequenceLayout(0,
                        MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE)).byteSize()),
                "java.lang.IllegalArgumentException: Element layout size is not multiple of alignment");
        eq("G14 seq(3,struct()) elementCount",
                MemoryLayout.sequenceLayout(3, MemoryLayout.structLayout()).elementCount(), 3L);
        eq("G15 seq(3,struct()) byteSize",
                MemoryLayout.sequenceLayout(3, MemoryLayout.structLayout()).byteSize(), 0L);
        eq("G16 seq(3,struct()) toString",
                MemoryLayout.sequenceLayout(3, MemoryLayout.structLayout()).toString(), "[3:[]]");
        eq("G17 seq(4,INT) elementCount",
                MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).elementCount(), 4L);
        eq("G18 seq(4,INT) byteSize",
                MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteSize(), 16L);
        eq("G19 struct(INT,null)",
                attempt(() -> MemoryLayout.structLayout(ValueLayout.JAVA_INT, null).byteSize()),
                "java.lang.NullPointerException: null");
        eq("G20 seq(4,null)", attempt(() -> MemoryLayout.sequenceLayout(4, null).byteSize()),
                "java.lang.NullPointerException: null");
        eq("G21 seq(-1,null) count wins",
                attempt(() -> MemoryLayout.sequenceLayout(-1, null).byteSize()),
                "java.lang.IllegalArgumentException: The provided elementCount is negative: -1");
        eq("G22 union(INT,null)",
                attempt(() -> MemoryLayout.unionLayout(ValueLayout.JAVA_INT, null).byteSize()),
                "java.lang.NullPointerException: null");

        System.out.println("checks=" + checks);
    }
}
```

---

## 13. What this lane did NOT do

* **It did not measure anything on CratonVM.** No build, no run, no
  `--dump-native-registry`, no `regression-suite`. Every "before" is read off
  the source it replaced; every "after" is a prediction. If the orchestrator's
  build disagrees with this record, the build is right.
* **It did not touch the native `get`/`set` alignment path** (§9.4), the
  session/scope model (§9.2), the closed-arena permissiveness (§9.1), or the
  heap access refusal's message decoration (§9.3).
* **It did not audit the downcall/upcall half of `panama.rs`** — libffi
  marshalling, `Linker`, `FunctionDescriptor`, `SymbolLookup`, trampolines. The
  layout carrier feeds those, and the slot-4 change is the only thing that
  reaches them; NOM-2 is the follow-up.
* **It did not verify `p67_layout_render` against the oracle in full.** Nine
  spellings match (§8.5); `1%i4`, `[i4(a)]`, `x4`, `[i4|j8]`, `[4:i4]`,
  `[2:[0:i4]]` and `[3:[]]` are transcribed in `P7`, but the renderer was only
  read, not exercised.
* **It did not fix NOM-1**, which is in another lane's file and which may make
  two existing tests red.
* **It changed no `INDEX.md` or `README.md`**, and created no files under
  `regression-suite/`.
