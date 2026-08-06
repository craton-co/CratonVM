# `java.nio.Buffer.address` is index 4, and the nio natives write index 4 as `mark`

| | |
|---|---|
| **Status** | **FIXED** for CharBuffer (2026-08-05, `0bf9d820d`) and for the ByteBuffer / typed-buffer families (this change). |
| **Scope** | `native-io/src/lib.rs` (`BB_FIELD_*`), `native-builtins/src/phases_late/charset_buffers.rs` (`CB_FIELD_*`), `native-builtins/src/charset.rs` (`BUF_FIELD_*`). |
| **Repro** | `docs/known-issues/repros/charbuffer-address/` — `BUFALL.java` sweeps every family and every mutator; `CB*.java` are the original CharBuffer cases. |

## The defect

`java.nio.Buffer`'s hierarchy-wide field order is

```
mark(0)  position(1)  limit(2)  capacity(3)  address(4)
```

then the subclass's own (`hb`, `offset`, `isReadOnly`). CratonVM's nio natives
keep a parallel **indexed** layout for synthetic mode:

```
*_FIELD_ARRAY=0  *_FIELD_POS=1  *_FIELD_LIMIT=2  *_FIELD_CAPACITY=3  *_FIELD_MARK=4
```

On a **real-JDK** buffer those indexed writes land on the real fields. Three of
them line up by luck. `*_FIELD_MARK = 4` does not — it lands on **`address`**.

That is not cosmetic. `Buffer.address` is what `ScopedMemoryAccess` /
`Unsafe.copyMemory` read for every bulk `put(<same-kind>Buffer)`. With
`address = -1` the computed offset falls below `arrayBaseOffset` (16), so
`unsafe_array_read_bytes`'s `byte_off.checked_sub(ABASE)` underflows and the
copy reports `ArrayIndexOutOfBoundsException`.

**The tell that localises it fast:** only `put(<same-kind>Buffer)` breaks.
`put(T[])`, `get(T[])` and `put(String)` all pass, because `put(Buffer)` is the
only bulk op that routes through `putBuffer` and therefore reads `address`.

```java
Field a = Buffer.class.getDeclaredField("address");  // needs --add-opens java.base/java.nio
a.setAccessible(true);
System.out.println(a.getLong(buf));   // HotSpot: 16.  Broken CratonVM: -1
```

Check it **after each mutator** — a fresh buffer reads 16 and `flip()` alone
takes it to -1.

## What was already fixed, and what this change adds

`charset.rs`'s ByteBuffer allocator carried the allocation-time half of the fix
from the start, with a comment warning that the indexed writes "can alias the
real `address` slot". Only the allocator was protected, so any later mutator
re-clobbered it. 2026-08-05 fixed the CharBuffer family the same way.

The audit that followed found the rest of it:

1. **`native-io`'s `buf_set_mark` clobbered `address` on every mutator.** It is
   the single helper behind `position`, `limit`, `mark`, `reset`, `clear`,
   `flip`, `rewind`, `compact` and `duplicate` for ByteBuffer *and* the typed
   buffers — 13 call sites, all of them writing the mark onto `address`.
2. **`alloc_typed_buffer` never wrote `address` at all**, so every
   Short/Int/Long/Float/Double/Char buffer it handed back started life with
   `address` reading as the mark (-1). Only `alloc_byte_buffer` had the
   allocation-time write.
3. **`alloc_mapped_byte_buffer` likewise never wrote it.** Its
   `MBB_FIELD_MAPPED_ADDR` (index 10) is CratonVM's own mapping id and is *not*
   the JDK's `address`.
4. **The CharBuffer fix hardcoded `address = 16`,** which is only right for a
   buffer whose `offset` is 0. A real `HeapCharBuffer` sets
   `address = ARRAY_CHAR_BASE_OFFSET + offset * 2`, so a **slice** carries a
   larger address and the flat constant corrupted it.

## Which of those actually execute — measure before believing

Two corrections to the paragraph above, both found by running the probe against
a binary that did **not** have the fix and getting `fails=0`:

* **`native-io`'s whole nio block is `#[cfg(feature = "synthetic-jdk")]`.**
  `register_nio_natives` is gated off in real-JDK mode on purpose — the comment
  at the gate says calling these overrides on a real instance "panics with a
  layout mismatch". So items 1–3 above are **latent**: `buf_set_mark` never sees
  a real-JDK `Buffer` today, and in synthetic mode there is no by-name `address`
  field for it to damage. They are fixed as hardening, so that the module's own
  dual-write intent holds if the gate is ever opened — not because they fail
  anything now. Do not quote them as a live bug.
* **The CharBuffer natives in `native-builtins` DO run in real-JDK mode**, which
  is why that half was a live defect (it broke every source file javac read).
  Item 4, the slice/offset half, is live for the same reason.

**`--dump-native-registry` answers "does this registration run?" directly** and
would have saved the round below. Its `invocations` field is per-registration,
so it names the *winner* of a duplicate triple, not just the last registrar. On
a `CBSLICE` run: **522** buffer registrations exist, **12** ever dispatched, and
every site fixed here is among them —

```
  1  java/nio/CharBuffer.wrap([C)Ljava/nio/CharBuffer;   by=charset_buffers.rs:874
  2  java/nio/CharBuffer.flip()Ljava/nio/CharBuffer;     by=charset_buffers.rs:1330
  1  java/nio/CharBuffer.clear()Ljava/nio/CharBuffer;    by=charset_buffers.rs:1341
  1  java/nio/CharBuffer.rewind()Ljava/nio/CharBuffer;   by=charset_buffers.rs:1352
```

while nothing from `native-io`'s nio block appears at all, which is the same
conclusion as the `cfg` gate above, arrived at by measurement.

**The probe was wrong twice before it measured anything.** `BUFALL.java` took a
`java.nio.Buffer` parameter, so `b.flip()` compiled to `invokevirtual
java/nio/Buffer.flip()Ljava/nio/Buffer;` — and the natives are registered on the
CONCRETE class with the concrete return descriptor
(`java/nio/CharBuffer.flip()Ljava/nio/CharBuffer;`), which that call site does
not match. It reported OK for every family because it was exercising the real
JDK throughout. It also only ever built buffers with `offset == 0`, which cannot
distinguish "preserved the real address" from "wrote the constant 16".
`CBSLICE.java` fixes both: concrete static types, and `slice()`/`wrap(a,off,len)`
sources with a non-zero offset.

## A separate defect the corrected probe found: `wrap` stamped the abstract class

`CharBuffer.wrap([C)` and `wrap(CharSequence)` allocated
`java/nio/CharBuffer` — the **abstract** class — while `allocate` (via
`p62_alloc_char_buffer`) and `subSequence` in the same file allocate
`java/nio/HeapCharBuffer`. Stamping the abstract class means any real-JDK method
*without* a native override dispatches to its abstract declaration:

```
CharBuffer.wrap(chars).slice()
  -> AbstractMethodError: method java/nio/CharBuffer.slice()Ljava/nio/CharBuffer;
     has no Code attribute
```

where HotSpot returns a buffer with `address = 216, offset = 100`. Fixed by
stamping `HeapCharBuffer` at both `wrap` sites.

**Was still open, now measured (2026-08-06):** `alloc_concurrent_synthetic(ctx,
"java/nio/ByteBuffer", …)` and `…"java/nio/CharBuffer"…` appear at ~18 further
sites (`servlet.rs`, `xnio_conduits.rs`, `phases_late/nio_file.rs`, `lib.rs`).
The paragraph above said each "has the same exposure the moment real-JDK
bytecode calls an unoverridden method on the result" and left it there.
`probes/NioBufferStampProbe` asked the question directly, against a HotSpot
control. The answer splits cleanly in two — see the next section.

## The CharBuffer view half: three live defects, fixed 2026-08-06

`ByteBuffer.asCharBuffer()` was the live one, and it was wrong three ways at
once. `probes/NioBufferStampProbe`, `--real-jdk`, against HotSpot 25:

```text
HotSpot   asCharBuffer kind=ByteBufferAsCharBufferB  slice=…B dup=…B ro=…RB
                       sub=…B put=ok str=32 bulk=ok
before    asCharBuffer kind=CharBuffer               slice=throw-AbstractMethodError
                       dup=throw-AbstractMethodError  ro=throw-AbstractMethodError
                       sub=throw-AbstractMethodError  put=throw-AbstractMethodError
                       str=throw-AbstractMethodError  bulk=throw-AbstractMethodError
after     identical to HotSpot
```

1. **The abstract stamp.** Seven of the eight methods the probe calls threw
   `AbstractMethodError`. `--dump-native-registry` says why: the abstract
   `java/nio/CharBuffer` carries 32 registrations, and `slice`, `duplicate`,
   `asReadOnlyBuffer`, `put(int,char)` and bulk `put(CharBuffer)` are not among
   them. Adding a native per method is whack-a-mole; a concrete receiver gets
   all of them from the JDK's own bodies.
2. **It was a copy, not a view.** `s2_bb_as_char_buffer` transcoded the bytes
   into a fresh `char[]`. So `bb.asCharBuffer().put('A')` left the backing
   ByteBuffer at `0,0,0,0` where HotSpot writes `0,65,0,66`. **A lost write is
   the worse of the two failures**, because nothing reports it — the
   `AbstractMethodError` at least names itself.
3. **`hasArray()` answered `true`** off that copy. A char view over a ByteBuffer
   has no accessible array.

The fix is one class name and one seeded field. `java/nio/ByteBufferAsCharBuffer{B,L}`
*already had* a native surface here (`charAt`/`get`/`hasArray`/`order`/`toString`,
reading through `bb`) — it was simply never what `asCharBuffer` returned. Two
notes worth keeping:

* **`class_id_by_name` is not an availability test.** The first attempt gated on
  it and silently kept the copying path, because nothing loads
  `ByteBufferAsCharBufferB` before the first `asCharBuffer` call. Drive the load
  with `ensure_class_initialized` — and then *still* check
  `is_class_synthetic_stub`, because that call fabricates a stub rather than
  failing (`ensure-class-initialized-fabricates-instead-of-failing`). A stub
  would trade `AbstractMethodError` for a buffer whose every method is a silent
  no-op.
* **`address` is the aliasing mechanism.** The JDK computes each element's byte
  offset as `(i << 1) + address` and seeds `address` to the source's address plus
  its position. `bbacb_read_underlying_bytes` was reading `bb.offset` instead,
  which drops the source's position (`bb.position(4).asCharBuffer().get(0)`
  decoded byte 0) and follows the source's position after the view is taken
  rather than freezing it.

The `subSequence` override on those two classes was **deleted** rather than
fixed: it existed to add the `Objects.checkFromToIndex` bounds check, and the
JDK's own body has that check by construction — now that the receiver is
concrete, the override could only shadow it.

## The ByteBuffer half: latent, and that is a measurement

`ByteBuffer.allocate` / `wrap` still stamp the abstract `java/nio/ByteBuffer`.
Every method the probe calls behaves **identically to HotSpot** — `slice`,
`duplicate`, `asReadOnlyBuffer` (read-only *is* enforced: `put` throws
`ReadOnlyBufferException` and `array()` throws), `compact`, `getInt`,
`equals`/`compareTo`/`hashCode`, bulk `put(Buffer)`, `mismatch`,
`alignedSlice`, `slice(int,int)`, `get(int,byte[])`, and
`StandardCharsets.UTF_8.decode(bb)`. The only divergence is `getClass()`:

```text
HotSpot   allocate kind=HeapByteBuffer   wrapSlice kind=HeapByteBuffer arrayOffset=8 address=base+off
CratonVM  allocate kind=ByteBuffer       wrapSlice kind=ByteBuffer     arrayOffset=8 address=base+off
```

That is not an accident — `register_essential_natives` says so out loud, and
registers the S2 ByteBuffer surface in real-JDK mode precisely so those abstract
methods have bodies. The exposure is real but conditional: it costs nothing
until someone reaches a ByteBuffer method nobody has written a native for.

**Not swept, deliberately.** Stamping `HeapByteBuffer` is the right end state,
but it hands the whole ByteBuffer surface over to real JDK bodies at once, and
`ByteBuffer` is reached by nearly everything. The CharBuffer half above is the
template for doing it: build the concrete class, seed `address` honestly, delete
the overrides that then only shadow. Anyone picking this up should start from
`probes/NioBufferStampProbe` — it already prints the `kind=` line that will flip.

## A missing native the probe found on the way

`FileChannel.transferFrom` between two file channels raised
`UnsatisfiedLinkError: sun/nio/ch/FileDispatcherImpl.transferFrom0` and copied
nothing, while `transferTo` in the same direction transferred all 4096 bytes —
`transferFrom0` simply had no registration. It now answers
`IOStatus.UNSUPPORTED`, which is what HotSpot's own implementation returns where
there is no kernel-side copy; `FileChannelImpl` falls back to
`transferFromArbitraryChannel`, a ByteBuffer loop that already works here, and
the probe reports `bytes=4096 identical=true` in both arms.

## The rule

* **Mutators save and restore.** They run on buffers CratonVM did not allocate,
  including slices (`address = base + offset * scale`) and **direct** buffers
  (`address` is a genuine native pointer that must never be synthesised).
  Preserving whatever the object already carries is correct for all three, and
  is a no-op in synthetic mode where the by-name field does not exist.
* **Allocators write the real value,** honouring `offset`.

## Testing note: the mock hid this

`MockNativeContext` keeps indexed and by-name fields in two independent maps —
which is exactly the property that makes this defect invisible. A `buf_set_mark`
test written against the default mock passes whether or not the fix is present.
`MockNativeContext::alias_nio_buffer_fields()` (opt-in, off by default) makes
slots 0..=4 alias `mark`/`position`/`limit`/`capacity`/`address` the way a
loaded `Buffer` subclass does. Verified as a negative control: with the aliasing
on, reverting the fix turns two of the four tests red; with the aliasing off,
all four stay green either way.
