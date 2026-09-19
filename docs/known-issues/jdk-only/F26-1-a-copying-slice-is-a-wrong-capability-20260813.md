# F26-1 — a copying `slice()` is a wrong CAPABILITY, the `offset` `duplicate()` dropped, and the constant that stopped being exact

**2026-08-13, lane F26.** Lands F21-1's **N4** (the storage model: derived
buffers COPIED where HotSpot aliases) and F21-1's **N3** (the typed families'
`isReadOnly` constant), plus one defect found while landing N4 that is **live on
dev today** (§3), one aliasing hole neither lane had looked at (§4), and
W7-76 §8.2's five-way duplicated field seed converged for the two files this
lane owns (§5).

**Provenance: MEASURED oracle, PREDICTED VM.** Every expected value below is a
pasted transcript from `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build) on this host. **This lane may not build or run CratonVM**, so
every "after" is a PREDICTION and is labelled as one. Probes:
`scratchpad/f26/F26AliasProbe.java` (the full aliasing sweep) and
`scratchpad/f26/W.java` (the `wrap` half and the composition rows).

What WAS executed, and it is not the VM: the four pure geometry functions this
record adds were extracted verbatim into `scratchpad/f26/geom.rs` and run under
`rustc --test` with their fixtures — 13 pass, and **11 mutants, one per wrong
implementation named below, all die** (§7). Both edited files were parse-checked
with `rustfmt` on a COPY (exit 0). Neither is a type-check and neither is a run.

Files changed: `native-io/src/lib.rs`, `native-builtins/src/servlet.rs`. Both are
this lane's. Everything else is a NOMINATION in §8 with exact old/new text.

---

## 1. The aliasing contract, MEASURED

`ByteBuffer.allocate(8)` filled `10..17`, `position(2)` where stated.
`array` is compared by **identity** (`==` against the source array), never by
equality — the whole point is which object comes back.

### 1.1 Heap, writable source

| derived from `hbb` | class | `isReadOnly` | pos/lim/cap | `hasArray` | `array()` | `arrayOffset()` | write through view seen by source |
|---|---|---|---|---|---|---|---|
| `.position(2).slice()` | `HeapByteBuffer` | false | 0/6/6 | true | **SAME** | **2** | **SHARED** (both directions) |
| `.slice(3,4)` | `HeapByteBuffer` | false | 0/4/4 | true | **SAME** | **3** | **SHARED** |
| `.duplicate()` | `HeapByteBuffer` | false | 2/8/8 | true | **SAME** | 0 | **SHARED** |
| `.asReadOnlyBuffer()` | `HeapByteBufferR` | true | 2/8/8 | false | `ReadOnlyBufferException` | `ReadOnlyBufferException` | **SHARED** (read-through) |
| `.slice().slice(1,2)` | `HeapByteBuffer` | false | 0/2/2 | true | **SAME** | **3** | — |
| `.asIntBuffer()` | `ByteBufferAsIntBufferB` | false | 0/2/2 | false | `UnsupportedOperationException` | `UnsupportedOperationException` | **SHARED** |
| `.position(4).asIntBuffer()` | `ByteBufferAsIntBufferB` | false | 0/1/1 | false | `UnsupportedOperationException` | `UnsupportedOperationException` | **SHARED** |
| `.asCharBuffer()` | `ByteBufferAsCharBufferB` | false | 0/4/4 | false | `UnsupportedOperationException` | `UnsupportedOperationException` | — |

Three properties a COPY cannot express, and each is load-bearing:

1. **`array()` is the source array.** Not an equal array — the same object.
2. **`arrayOffset()` composes.** `slice()` gives 2; `slice().slice(1,2)` gives
   **3**, i.e. `index + offset`, and `allocate(4).position(4).slice()` gives
   **4** even though the slice is EMPTY. So the offset moves with `position`
   unconditionally.
3. **Content is shared in BOTH directions.** A write through the slice is
   visible through the source and vice versa.

### 1.2 Heap, read-only source (`allocate(8).asReadOnlyBuffer()`)

Every derived buffer is `HeapByteBufferR`, `isReadOnly == true`,
`hasArray == false`, and BOTH `array()` and `arrayOffset()` raise
`ReadOnlyBufferException` — for `slice()`, `slice(3,4)`, `duplicate()` and
`asReadOnlyBuffer()` alike. `asIntBuffer()` gives `ByteBufferAsIntBufferRB`,
`isReadOnly == true`, and its two accessors raise
`UnsupportedOperationException` (**not** `ReadOnlyBufferException` — the
array-less arm is checked FIRST).

**Read-only does not mean isolated.** Measured on a writable buffer and its
read-only views:

```text
w.put(5) seen by its aro                       SHARED
w.put(6) seen by aro.duplicate()               SHARED
w.put(2) seen by aro.pos(2).slice().get(0)     SHARED
```

So a read-only view is a *view*: it forbids writing THROUGH ITSELF and shares
storage in every other respect.

### 1.3 Direct

| derived from `allocateDirect(8)` | class | `isDirect` | `isReadOnly` | pos/lim/cap | `array()`/`arrayOffset()` | shared |
|---|---|---|---|---|---|---|
| `.position(2).slice()` | `DirectByteBuffer` | **true** | false | 0/6/6 | `UnsupportedOperationException` | **SHARED** |
| `.slice(3,4)` | `DirectByteBuffer` | **true** | false | 0/4/4 | `UnsupportedOperationException` | — |
| `.duplicate()` | `DirectByteBuffer` | **true** | false | 2/8/8 | `UnsupportedOperationException` | **SHARED** |
| `.asReadOnlyBuffer()` | `DirectByteBufferR` | **true** | true | 2/8/8 | `UnsupportedOperationException` | **SHARED** |
| `.asIntBuffer()` | `DirectIntBufferS` | **true** | false | 0/2/2 | `UnsupportedOperationException` | — |

`allocateDirect(8).asReadOnlyBuffer()` derives `DirectByteBufferR` for all of
`slice`/`duplicate`/`asIntBuffer` (`DirectIntBufferRS`), all `isDirect == true`.

**`isDirect()` is preserved by every derivation.** A derived buffer that came
back as a heap COPY answers `false` where HotSpot answers `true` — a caller
that branches on `isDirect()` to pick a zero-copy path takes the wrong branch.

### 1.4 `wrap`, and the typed families

```text
wrap(arr).array()==arr                  : true        <- IDENTITY
wrap(arr).arrayOffset()                 : 0
wrap(arr).pos/lim/cap                   : 0/8/8
put(0,0x7F) -> raw[0]                   : 127         <- write reaches the CALLER's array
raw[1]=0x11 -> b.get(1)                 : 17          <- and the other direction
wrap(raw,2,3).array()==arr              : true
wrap(raw,2,3) pos/lim/cap               : 2/5/8       <- off/len move POSITION and LIMIT
wrap(raw,2,3).arrayOffset()             : 0           <- and NOT the array base
wrap(raw,0,9)                           : java.lang.IndexOutOfBoundsException msg=null
wrap(raw,-1,2)                          : java.lang.IndexOutOfBoundsException msg=null
```

`IntBuffer.allocate(8)` behaves exactly as §1.1's byte rows (`slice()` →
`arrayOffset() == 2`, `array()` SAME, `slice(3,4)` → 3, `duplicate()` → 0,
`asReadOnlyBuffer()` → `HeapIntBufferR` and both accessors
`ReadOnlyBufferException`), and `tw.slice().put(0,999)` is visible as
`tw.get(2) == 999`.

### 1.5 Composition, and the controls

```text
wrap(raw,2,4).slice().arrayOffset()          : 2
wrap(raw,2,4).slice().duplicate().arrayOffset(): 2      <- duplicate CARRIES the offset
wrap(raw,2,4).slice().duplicate().get(0)     : raw[2]
allocate(4).position(4).slice().arrayOffset(): 4        <- empty slice, offset still moves
slice(2,4) then position(1) then compact()   : writes land in raw[2..], at the offset
duplicate carries mark                       : 3
slice carries mark                           : java.nio.InvalidMarkException
LE bb.slice().order()                        : BIG_ENDIAN
LE bb.duplicate().order()                    : BIG_ENDIAN
LE bb.asIntBuffer().order()                  : LITTLE_ENDIAN
```

The last four reproduce F21-1's measurements exactly and are reproduced here
only as controls; they were not re-derived as findings.

---

## 2. What the VM did, and why "copy" is a CAPABILITY defect

`native_bb_slice` (`native-io/src/lib.rs`) allocated a fresh buffer and copied
the remaining bytes into it. Consequences, each a row of §1.1 that a copy cannot
reach:

* `hbb.slice().array()` returned a **6-byte private array**, not the source's
  8-byte one. `==` fails; `.length` differs.
* `hbb.position(2).slice().arrayOffset()` returned **0**, *unreachably* — a
  copying implementation has no value it could return but 0.
* **Writes through the slice were lost.** `slice()` is documented as "changes to
  this buffer's content will be visible in the new buffer, and vice versa". A
  program that slices a buffer, hands the slice to a decoder and reads the
  result back through the original got the ORIGINAL bytes, with no exception at
  any point. That is not a wrong answer; it is a **missing capability**, and it
  is the quiet kind — nothing reports it.
* A DIRECT source came back as a HEAP copy, so `isDirect()` flipped
  `true` → `false` as well.

**Why the repair had to land in `native-io` and not in `servlet.rs`, which
already had an aliasing implementation of this exact descriptor.** F21-1
established this and this lane RE-VERIFIED it rather than inheriting it:
`vm/src/vm/vm_init.rs` calls `register_essential_natives_with_shims` at L2055
and L2593 and `register_io_natives` at L2252 and L2788 — later in both real-JDK
arms — and registration is last-write-wins. So for every descriptor BOTH
registrars claim, native-io wins. `servlet.rs`'s aliasing `slice()`/`duplicate()`
were shadowed and never ran.

---

## 3. The live bug this uncovered: `duplicate()` shared the array and dropped the `offset`

**This one is not hypothetical and is not caused by this lane's change.**

F21 taught `native_bb_duplicate` to share the source's backing array (closing
half of the same defect) but wrote nothing to `ByteBuffer.offset`, so the
duplicate inherited the fresh allocation's `0`. F21's own note argued this was
safe because nothing could produce a source with a non-zero `offset`.

**The premise is false, and the counterexample is in the other registrar.**
`register_nio_natives` registers `ByteBuffer.slice()` and `duplicate()` — it
registers **neither `slice(int,int)` nor `asReadOnlyBuffer()`**. So for those
two descriptors `servlet.rs` is the only registration and it WINS, and both call
`s2_bb_new_heap_view`, which writes `offset` by name. Therefore:

```java
bb.slice(2, 4).duplicate().get(0)   // read arr[0]; HotSpot reads arr[2]
```

A silent WRONG VALUE from a two-call sequence of documented API, on dev, today.
MEASURED on HotSpot: `wrap(raw,2,4).slice().duplicate().arrayOffset()` is `2`
and its `get(0)` equals `raw[2]`.

The same body also turned every DIRECT source into a heap copy (its `else` arm
fired for every `allocateDirect(n)` receiver, which has no `hb`), so
`allocateDirect(8).duplicate().isDirect()` answered `false` where HotSpot
answers `true`, and writes stopped being shared.

---

## 4. `ByteBuffer.wrap` copied the caller's array — neither prior lane looked at it

`native_bb_wrap` and `native_bb_wrap_range` copied every byte into a private
array. `ByteBuffer.wrap(bytes)` is the single most common ByteBuffer idiom and
its entire contract is "a buffer view OF THESE BYTES"; HotSpot's is literally
`new HeapByteBuffer(array, array.length, null)`. Both directions were broken and
both silently:

* fill the buffer, then read the ARRAY → zeros;
* mutate the array, then read through the buffer → stale bytes;
* `wrap(arr).array() == arr` → false.

`servlet.rs`'s `wrap` DID alias (`bb_write_hb(ctx, buf, arr, len)`), and is
shadowed by native-io's for exactly the §2 reason. So this is a third instance
of F21-1's registrar finding, in a method neither F14 nor F21 examined.

`wrap_range` additionally had **no bounds check at all**. That was survivable
while the body copied (an over-long `limit` merely pointed past the end of a
private array); with the array now the caller's, an unchecked limit indexes past
the end of a REAL array on every later read. So the check is part of the same
change, not scope creep. MEASURED: both `wrap(raw,0,9)` and `wrap(raw,-1,2)`
raise `java.lang.IndexOutOfBoundsException` with a **null** message —
`RuntimeError::ioobe_no_message()`, the spelling `servlet.rs`'s twin already
uses.

---

## 5. The N3 constant, and the width claim checked rather than inherited

`servlet.rs` registered `isReadOnly()Z` as a flat `Ok(Some(Value::Int(0)))` for
`java/nio/{Int,Long,Short,Float,Double}Buffer`. Its comment argued the constant
was EXACT — `$ro` aliased `$dup`, so no read-only view of such a buffer could
exist — and **that argument was sound when it was written**.

**F21 falsified the premise from the other crate.** `native-io`'s
`tb_abstract_view_fns!` `$ro_fn` now stamps `isReadOnly = 1` by name, and by §2's
ordering it is native-io's `asReadOnlyBuffer` that answers for these five
classes. Read-only typed views exist. Meanwhile `hasArray`/`array`/`arrayOffset`
all read the flag through `native_io::buffer_array_access`. So the receiver was
in the exact impossible state that got `native_bb_is_read_only` fixed:
`hasArray() == false`, `array()` throwing `ReadOnlyBufferException`, and
`isReadOnly()` reporting the buffer writable — steering a caller that branches on
`isReadOnly()` rather than `hasArray()` straight into the throw.

MEASURED: `allocate(32).asReadOnlyBuffer().asIntBuffer()` is
`java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`; same for
Long/Short/Float/Double/Char and for the direct twin `DirectIntBufferRS`.

**The width claim, checked.** The old comment said this needs "a wider
allocation" because "the 6-slot synthetic is full". True of the synthetic layout
and **irrelevant to this registration**, in both directions:

* Real-JDK mode: `javap -p java.nio.IntBuffer` on 25.0.3+9 gives
  `int[] hb; int offset; boolean isReadOnly;` — `isReadOnly` is a DECLARED
  field, identical on Long/Short/Float/Double. And `VmExec::alloc_object`
  (`vm/src/vm/vm_exec.rs`, "Layout-mismatch guard") clamps a native allocator's
  requested slot count **UP** to the resolved class's declared field count. The
  field is present on every such receiver and the by-name read resolves.
  Nothing needs widening.
* Synthetic-JDK mode: no such field, the by-name read yields a non-`Int`, and
  `s2_bb_is_read_only` answers `false` — byte-for-byte the constant it replaces.

So the fix is behaviour-preserving in the mode the width argument was about and
correct in the other. The widening the old comment called for is needed only to
make synthetic-mode views *actually* read-only, which is a different job.

**DISCLOSED RESIDUAL, deliberately not landed:** `$put` / `$put_abs` /
`$put_bulk` / `$compact` in `s2_typed_buffer_view_fns!` still do not consult the
flag, so a write through a read-only typed view succeeds where HotSpot raises
`ReadOnlyBufferException` (MEASURED:
`allocate(32).asReadOnlyBuffer().asIntBuffer().put(0,1)` →
`java.nio.ReadOnlyBufferException`). Making the accessor honest cannot be a step
away from that: it moves one more accessor onto the flag the enforcement will
read.

---

## 6. Task 3 — W7-76 §8.2's five-way seed, converged for the two owned files

W7-76 §8.2 named five sites seeding `bigEndian`/`nativeByteOrder` "that must
agree and that nothing makes agree". Two are this lane's and now delegate to one
exported helper, `native_io::seed_buffer_byte_order`; the same treatment is
applied to `ARRAY_BYTE_BASE_OFFSET`, which both crates spelled as an independent
`16` while both use it as the heap sentinel their `address` screens compare
against. `servlet.rs`'s const is now DEFINED AS native-io's (`const X: i64 =
cratonvm_native_io::X;` — const-evaluated, all nine local use sites untouched,
drift impossible).

The other three sites are §8 nominations. One of them is already visibly
drifted: `charset.rs` writes `bigEndian` and **not** `nativeByteOrder`.

---

## 7. What was executed, and the mutants

The four pure functions (`slice_window`, `slice_range_window`,
`duplicate_window`, `heap_buffer_address`) and their 13 fixtures were extracted
verbatim into `scratchpad/f26/geom.rs` and run under `rustc --test`: **13
passed.** They pin geometry, not a receiver — `MockNativeContext`'s
name-to-slot fallback would make an assertion about `arrayOffset()` a
measurement of the mock's field table rather than of the model.

Mutation check, one mutant per wrong implementation the record names —
**all 11 died**:

| mutant | dead |
|---|---|
| `slice` offset drops the source's `offset` | 2 tests |
| `slice` offset drops `position` (**= the copying body's only reachable answer**) | 7 tests |
| `slice` capacity is the source's `limit` | 4 tests |
| `slice` keeps the source position | 2 tests |
| `slice(index,length)` drops the source's `offset` | 2 tests |
| **`duplicate` drops the offset (= the shipped body, §3)** | 1 test |
| `duplicate` drops the mark | 1 test |
| `duplicate` capacity is the limit | 1 test |
| `heap_buffer_address` is a bare `16` | 2 tests |
| `slice` does not clamp a negative position (`usize` wrap) | 1 test |
| `slice` of an inverted range goes negative | 1 test |

Both edited files parse: `rustfmt --edition 2021 --emit stdout` on a COPY, exit
0 for each, with **no formatting drift in any F26-authored line**. That is a
parse, not a type-check.

---

## 7a. The compile gate went RED on this lane's own line, and the attribution

`cargo` reported one error, E0502 in `buf_stamp_read_only`:

```
cannot borrow `*ctx` as immutable because it is also borrowed as mutable
buf_write_read_only(ctx, derived, derivation, buffer_is_read_only(ctx, src));
```

**This is F26's line, not F21's, and the distinction is worth stating so nobody
goes looking for more F21 damage.** F21's body was

```rust
let ro = buffer_view_read_only(derivation, buffer_is_read_only(ctx, src));
ctx.set_field_by_name(derived, "isReadOnly", Value::Int(i32::from(ro)));
```

— `buffer_view_read_only` is PURE and takes no `ctx`, so only one borrow ever
existed. The overlap appeared when §*this lane* made `buf_stamp_read_only`
delegate to the new `buf_write_read_only`, which takes `ctx` as `&mut dyn`
while `buffer_is_read_only` takes it as `&dyn`. Fixed by binding the read
first.

**Is the hoist semantically safe?** It is not a hoist. Rust evaluates arguments
left to right, so `buffer_is_read_only(ctx, src)` was ALREADY evaluated before
`buf_write_read_only`'s body ran; the binding only ends the shared borrow
earlier. Nothing moved across anything. Independently: the two points are
ADJACENT (no call between them), `buffer_is_read_only` is a single
`get_field_by_name` with no allocation and no re-entry into Java, and the only
way read-order could matter at all is `src == derived` — which no caller does.
All four `tb_abstract_view_fns!` sites pass `this` plus a buffer
`alloc_typed_buffer` has just minted, and `$ro_fn` passes `$dup_fn`'s fresh
result.

**Siblings, swept rather than assumed.** Every call site of the family
(`buffer_is_read_only`, `s2_bb_is_read_only`, `cb_is_read_only`,
`buf_stamp_read_only`, `buf_write_read_only`) across `native-io/src/lib.rs`,
`servlet.rs` and `charset_buffers.rs` was inspected. All the others are one of
four shapes that cannot produce E0502: nested inside the PURE
`buffer_array_access`; a standalone `if` condition whose borrow ends before the
body mutates; an already-bound `let ro = …`; or a single `ctx` argument.
**The four `tb_abstract_view_fns!` sites the orchestrator flagged pass `ctx`
exactly once and were never at risk.** The compiler's "1 error" was accurate.

**VERIFIED, not asserted.** `scratchpad/f26/bck.rs` reproduces all eight call
shapes this lane wrote against a stand-in `NativeContext` trait and compiles
(`rustc --edition 2021`, exit 0). The NEGATIVE CONTROL matters more: restoring
the nested form in that same harness reproduces `error[E0502]: cannot borrow
`*ctx` as immutable because it is also borrowed as mutable` verbatim. So the
harness is measuring the thing that failed, not something adjacent to it. It is
a borrow-check proof only — still not a type-check of the real crate.

**What that gate did NOT cover.** `cratonvm-native-builtins` depends on
`cratonvm-native-io`, so when native-io failed, `servlet.rs` was never compiled
— "everything else in the workspace compiled" cannot include it. Its two edited
shapes (the closure reborrow `i32::from(s2_bb_is_read_only(ctx, this))` and the
cross-crate `const X: i64 = cratonvm_native_io::X;`) are in the harness above
and compile there, but the gate has not seen them.

---

## 8. NOMINATIONS (exact text; not this lane's files)

### N1 — `native-io/src/direct_buffer.rs`, `dbb_allocate_direct0` (W7-76 §8.2)

OLD (at `direct_buffer.rs:743`):
```rust
    ctx.set_field_by_name(buf, "bigEndian", Value::Int(1));
    ctx.set_field_by_name(
        buf,
        "nativeByteOrder",
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 }),
    );
```
NEW:
```rust
    // CONVERGED (F26-1 §6, closing W7-76 §8.2). Same crate as the helper, so
    // this is a plain path call, not a cross-crate one.
    crate::seed_buffer_byte_order(ctx, buf);
```

### N2 — `native-builtins/src/charset.rs` (W7-76 §8.2, and already DRIFTED)

This site writes `bigEndian` and **not** `nativeByteOrder`, which is exactly the
drift §8.2 predicted.

OLD (at `charset.rs:259`):
```rust
    ctx.set_field_by_name(obj, "bigEndian", Value::Int(1));
```
NEW:
```rust
    // CONVERGED (F26-1 §6, closing W7-76 §8.2). This site wrote `bigEndian`
    // and NOT `nativeByteOrder` — the drift §8.2 predicted, now closed by
    // construction rather than by a second literal.
    cratonvm_native_io::seed_buffer_byte_order(ctx, obj);
```

### N3 — `native-builtins/src/lib.rs` (W7-76 §8.2)

OLD (at `native-builtins/src/lib.rs:5690`):
```rust
    ctx.set_field_by_name(this, "bigEndian", Value::Int(1));
    ctx.set_field_by_name(
        this,
        "nativeByteOrder",
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 }),
    );
```
NEW:
```rust
    // CONVERGED (F26-1 §6, closing W7-76 §8.2).
    cratonvm_native_io::seed_buffer_byte_order(ctx, this);
```

### N4 — `vm/src/runtime/interpreter/native_override.rs`: two descriptors missing from the force-native list

`("slice", "(II)Ljava/nio/ByteBuffer;")` and
`("asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;")` are **absent** from the
`java/nio/ByteBuffer` arm at L3184-L3255, while `slice()`/`duplicate()` are
present. That asymmetry is what makes §3's defect reachable, and it also means a
real-JDK receiver runs JDK bytecode for those two and native code for the other
two. This is a DIAGNOSIS, not a proposed edit — adding names to this arm is
explicitly gated by the comment above it ("Do NOT re-add a name here without
also re-adding the eight probes"), and this lane cannot run those probes. Whoever
takes it should decide the direction deliberately: either force all four, or
force none and let §2's whole registrar question be re-asked.

---

## 9. Residuals this lane did NOT land, stated as specification

### 9.1 The six typed families still COPY (`native-io`, `tb_abstract_view_fns!`)

`$slice_fn`/`$slice2_fn`/`$dup_fn` allocate a fresh typed buffer and copy
elements. MEASURED, they should alias exactly as §1.4's `IntBuffer` rows show.
**Not landed, and not because of effort.** The `ByteBuffer` repair works because
one field (`ByteBuffer.offset`, in BYTES) expresses the whole window. The typed
families need TWO mechanisms, chosen per receiver:

* a `HeapIntBuffer`-shaped receiver aliases through `hb` + `offset` in
  **ELEMENTS** (`int offset` on `java.nio.IntBuffer`, `javap`-verified), so
  `alloc_byte_buffer_over`'s byte arithmetic is wrong for it by a factor of the
  element width;
* a `ByteBufferAsIntBufferB`-shaped receiver — what `ByteBuffer.asIntBuffer()`
  returns — has **no `hb` at all**; it aliases through `bb` + `address`, which
  is the mechanism `servlet.rs`'s `s2_view_buf_fn!` already uses, and a third
  encoding (`-(byte_start + 1)` in the mark slot) is used by the s2 typed views.

Landing one of the three and calling the family fixed is the "diff a family fix
against every member" failure. The correct shape is a `TbWindow` sibling of
`BufferWindow` carrying an element-scaled offset plus an explicit storage-kind
discriminator, and a `tb_derive_view` that refuses (falls back to the copy)
rather than guessing when the receiver's encoding is not one it knows.

### 9.2 Read-only enforcement on typed-view writes

§5's disclosed residual: `$put`/`$put_abs`/`$put_bulk`/`$compact` in
`s2_typed_buffer_view_fns!` must raise `ReadOnlyBufferException` when
`s2_bb_is_read_only` holds. Landing it needs synthetic-mode views to be able to
CARRY the flag, which is the widening the old comment described. Not landed:
adding a new throw on four write paths across five classes, in a lane that
cannot run the suite, is how a green family goes red.

### 9.2a The typed macro dereferences its receiver AFTER allocating — deliberately NOT half-fixed

All four `tb_abstract_view_fns!` bodies call
`buf_stamp_read_only(ctx, this, new_buf, …)` *after* `alloc_typed_buffer`, so
`this` — a bare `ObjectRef` — crosses an allocation that can relocate it. That
is the hazard `native_bb_slice`/`native_bb_duplicate` now avoid by reading the
flag into a `bool` first, and the one-line fix would transplant cleanly.

**It was not applied, and the reason is not effort.** The same functions also
carry `view` (which holds an `ObjectRef` inside `BbStorage::Heap`) across that
allocation and read the buffer's DATA through it in the copy loop, and
`$dup_fn` reads `mark` through `this` as well. Hoisting only the read-only flag
would leave the larger exposure untouched while making the site LOOK
GC-correct — the "a fix that only pins the positive half hides what it
unmasked" shape. The correct repair pins `this` and re-reads `view` after the
allocation, at all four sites, and belongs to whoever takes §9.1 (the typed
families' aliasing), because that rewrite replaces the copy loop that creates
most of the exposure.

### 9.3 An aliasing view's `address` and the native-pointer screen

`heap_buffer_address(offset)` is `16 + offset`, matching real `HeapByteBuffer`.
`is_plausible_native_addr`'s floor is 65 536, so a view at an array-base offset
of **65 520 or more** presents an `address` the screen ACCEPTS. It is inert
today only because `bb_storage_view` resolves `hb` FIRST and an aliasing view
always has one. This is now a TEST
(`a_heap_view_address_stays_below_the_native_pointer_floor_for_ordinary_offsets`)
rather than a sentence, so a future reordering of those two checks fails a test
instead of dereferencing an array index as a pointer. The same bound applies
unchanged to `servlet.rs`'s pre-existing `s2_bb_new_heap_view`.

### 9.4 From the constant-where-state sweep of both files

The sweep asked for in the brief covered both owned files. Beyond §5's subject
it found, in severity order:

1. **`asReadOnlyBuffer()` on the five typed classes returns a WRITABLE alias** —
   `servlet.rs`'s `$ro` is `$dup(ctx, args)`. Shadowed by native-io's fixed
   `$ro_fn` per §2, so it is dead in real-JDK mode; it is the live producer in
   any configuration where the s2 registration wins. Same repair as §9.2.
2. **`FileChannel.size()J` is a flat `Ok(Some(Value::Long(0)))`**
   (`native-io/src/lib.rs`, `native_fc_size`) — it resolves the fd and discards
   it (`let _ = fd_id;`). Real `FileChannelImpl.size()` fstats. Callers size
   reads and `map()` regions from this, so every such path sees an empty file.
   A competing `p57_fc_size` exists in `phases_late/nio_file.rs`; which wins is a
   registrar-order question of exactly the §2 species and should be settled by
   reading `vm_init.rs`, not assumed.
3. **`FileChannel.open(Path, OpenOption[])` ignores the options** and always
   opens for read (`// Simplified: open for read`), so `open(p, WRITE, CREATE)`
   yields a read-only fd. Same 200 lines as (2).
4. **`WatchEvent.count()I` → constant `1`**, registered with `|_ctx, _args|` so
   it cannot read state. JDK: "If the event count is greater than 1 then this is
   a repeated event"; for `OVERFLOW` it is the number of events LOST. Right by
   luck today (the `WE_FIELD_*` layout has no count slot and nothing coalesces),
   so the fix needs a field before the accessor.
5. **`ServiceLoader.toString()` → constant `"ServiceLoader[]"`** (`servlet.rs`,
   `|ctx, _args|`). JDK 25 `ServiceLoader.java:1695` is
   `"java.util.ServiceLoader[" + service.getName() + "]"` — wrong on both the
   prefix and the name. Cheap, but the fix depends on which slot holds the
   service mirror; `reload()` clears slot 1, which is a hint and not a
   verification. **Verify the slot before writing the accessor** — a
   plausible-looking wrong slot here is the `_ =>`-default shape.
6. Trap, not yet a bug: `native_bis_mark_supported` (`|_ctx, _args| → 1`,
   `#[allow(dead_code)]`, unregistered). Exact for
   `BufferedInputStream.markSupported()`; **wrong** for `PipedInputStream`,
   which shares this `native_bis_*` family and inherits
   `InputStream.markSupported()` → `false`. Check the receiver set before wiring
   it.

Checked and CLEAN, recorded so they are not re-flagged:
`native_completed_future_*` (exact for the synthetic always-completed future),
`sun/nio/ch/NativeSocketAddress`'s `AFINET`/`sizeof*`/`offset*` (compile-time
ABI constants, which is what the real JNI does),
`UnixNativeDispatcher.init()I` → 0 (documented "no capabilities"),
`StringReader.markSupported()` → 1 (the JDK's is literally `return true;`),
`InputStream.available()` → 0 (the base-class default), and every one of
`ByteBuffer.isReadOnly`/`hasArray`/`isDirect`/`arrayOffset`,
`FileLock.isShared`/`isValid`, `WatchKey.isValid`,
`DatagramChannel.isBlocking`, `MappedByteBuffer.isLoaded` — all read the
receiver's field.

---

## 10. Which checks FLIP and which merely become REACHABLE (predicted)

**Flip** (a currently-passing assertion would now report a different value, or a
currently-failing one starts passing): any assertion on `ByteBuffer.wrap(a)
.array() == a`, on `slice().arrayOffset()`, on write-through visibility across
`slice`/`duplicate`/`wrap`, on `allocateDirect(n).duplicate().isDirect()`, and
on `IntBuffer.allocate(n).asReadOnlyBuffer().isReadOnly()`. `wrap(a, off, len)`
with an out-of-range range now THROWS where it silently returned a buffer.

**Merely reachable** (no assertion changes, but a code path that could not run
before now can): the aliasing arm of `bb_derive_view` in synthetic-JDK mode —
`buf_try_set_heap_offset` reports base 0 representable in every layout, so
`wrap` aliases there too, while a non-zero-offset `slice()` still takes the copy
fallback. Nothing in synthetic mode changes its answer; a different branch
produces it.

**Unchanged by construction:** `order()` on derived buffers (still reset to
BIG_ENDIAN; the seed moved but its value did not), `mark` on `duplicate` vs
`slice`, and the whole read-only contagion table — all F21-1's, reproduced here
only as controls.
