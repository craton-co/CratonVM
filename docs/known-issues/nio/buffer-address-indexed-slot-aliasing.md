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
