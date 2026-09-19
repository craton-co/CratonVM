# WORKER-4-NOTE-7 — a direct little-endian `FloatBuffer` silently loses every write, and it bottoms out in `Unsafe` with a NULL base

**Status: MEASURED, DIAGNOSED TO A LINE, NOT FIXED — the fix is in the `Unsafe`
layer and has a design question in it.** 2026-08-22, Linux (Azure host 2),
Temurin 25.0.4+7, handoff tip `8104bfd2f`.

## 1. The defect

```java
ByteBuffer.allocateDirect(16).order(ByteOrder.LITTLE_ENDIAN)
          .asFloatBuffer().put(0, 1.0f);
```

writes **nothing**. Read it back through the view: `0.0`. Read it through the
parent: `0.0`. Dump the parent's bytes: all zeros. No exception.

`DoubleBuffer` is the same. Every other width is fine, and so is every other
combination of backing and order:

```text
                       readBack   parent bytes
  heap   BIG_ENDIAN      1.0      3f80000000000000   ok
  heap   LITTLE_ENDIAN   1.0      0000803f00000000   ok
  direct BIG_ENDIAN      1.0      3f80000000000000   ok
  direct LITTLE_ENDIAN   0.0      0000000000000000   WRONG
```

Silent data loss, in the one arm of four.

## 2. Why only that arm — the class name says it

The JDK compiles one concrete view class per (backing x order), and the suffix
is the whole story:

```text
  heap   BIG_ENDIAN      java.nio.ByteBufferAsFloatBufferB
  heap   LITTLE_ENDIAN   java.nio.ByteBufferAsFloatBufferL
  direct BIG_ENDIAN      java.nio.DirectFloatBufferS      (Swapped   = non-native)
  direct LITTLE_ENDIAN   java.nio.DirectFloatBufferU      (Unswapped = native)
```

CratonVM reports all four class names, orders and capacities **correctly** —
this is not a receiver-identification problem. The two direct classes reach
different primitives:

* `DirectFloatBufferS` must swap, so the JDK routes it through
  `ScopedMemoryAccess.{get,put}IntUnaligned(session, base, addr, value,
  bigEndian)` — which this VM implements arena-aware. MEASURED:
  `getIntUnaligned` fires 4 times in the repro.
* `DirectFloatBufferU` needs no swap, so the JDK calls **plain
  `UNSAFE.putFloat(null, address, x)`**. MEASURED: `jdk/internal/misc/Unsafe
  .putFloat(Ljava/lang/Object;JF)V` fires exactly once, and the value vanishes.

**No `java.nio` native is involved at all.** The only `FloatBuffer` row that
fires in the whole repro is `session()`. So this is not a `native-io` defect
despite surfacing as one, and a lane looking for it in the buffer natives will
not find it.

## 3. The line

`native-builtins/src/unsafe_natives_ext.rs::native_unsafe_put_float`:

```rust
let obj = match unsafe_obj(args, 1) {
    Some(o) => o,
    None => {
        let _ = unsafe_static_put(ctx, offset, val);   // <- a STATIC-FIELD side table
        return Ok(None);
    }
};
```

**A null base means "off-heap absolute address", and this treats it as a
static-field key.** The write goes into a side table nothing will ever read it
back from; `native_unsafe_get_float`'s matching arm reads that same table and
answers `0.0`.

The two-argument form of the same operation gets it right, thirty lines away in
`unsafe_natives.rs`:

```rust
registry.register(class, "putFloat", "(JF)V", |_ctx, args| {
    …
    if crate::unsafe_arena_put_int(addr, v.to_bits() as i32) {
        refresh_arena_cache(addr, 4);
        return Ok(None);
    }
    Err(… "address 0x{addr:x} is not in any live arena")
});
```

So the VM knows how to do this. `putFloat(J,F)` writes the arena and REFUSES
loudly off it; `putFloat(Object,J,F)` with a null base writes a side table and
refuses nothing.

## 4. Why this is reported rather than fixed

The obvious patch — try the arena first in the null-base arm, fall back to the
static store — has a real design question in it, and it is not mine to answer:

**both interpretations of a null base are legitimate in this codebase.**
`putInt`'s and `putLong`'s null-base arms deliberately fall back to a static
side store (`static_int_store()`), which exists because some callers reach
`Unsafe` with a null base and a STATIC-FIELD offset. Ordering the two
interpretations wrong corrupts the other one: a static-field key that happens to
collide with a live arena address would be written to memory, or an arena
address that happens to match a static key would be swallowed exactly as it is
today.

Answering that needs the `Unsafe` layer's own inventory of who passes a null
base and what they mean by the offset — which is a different lane's file and the
single most load-bearing native surface in the VM. `[a refusal with evidence
beats a retirement without it]`, and this is the same principle applied to a fix.

**What a lane taking it should know:**

* the discriminator probably already exists — `unsafe_arena_put_int` returns
  `false` for an address that is not in a live arena, which is exactly the
  "this is not off-heap memory" signal the ordering needs;
* `putFloat`, `putDouble`, `getFloat` and `getDouble` are the four bodies that
  discard the result (`let _ = unsafe_static_put(…)`) rather than testing it,
  and are the minimum surface;
* `putInt`/`putLong`/`getInt`/`getLong` share the shape and would lose writes
  the same way — they are simply never reached with a real address today,
  because the JDK routes their direct-buffer paths through
  `ScopedMemoryAccess`, which is arena-aware. **That makes them a latent
  instance of the same defect, not a working case.**

## 5. The gate

`regression-suite/probes/W4Views.java` (added here) — 123 cases over six widths
x two backings x two byte orders, plus wrapped standalone views. It reports 6
diffs today: the four float/double ones above and two more
(`CharBuffer.toString()` on a direct-backed char view answers `""`, both
orders — §6). `W4FloatLE.java` is the six-line isolation, printing the view
class, order, capacity, both read paths and the parent's raw bytes.

Two things about the probe's shape that are the reason it found this:

* **the load-bearing assertion is WRITE-THROUGH to the parent's bytes**, read as
  hex, not a read-back through the same view. A read-back cancels a symmetric
  bug; this one is not symmetric, but the next one might be.
* **the four arms are isolated.** The first run of this probe died at
  `direct.be.char.toString()` — a bare `.substring(0, 3)` on the `""` from §6 —
  and took every later case with it, including all four float/double diffs.
  One wrong answer hid five others. Each arm now catches, and `toString()` is
  read defensively because it is itself under test.

## 6. The second defect the same probe found

```text
  ByteBuffer.allocateDirect(16).asCharBuffer().toString()
    HotSpot    "Aé中"     (the chars between position and limit)
    CratonVM   ""         both byte orders
```

The same view's `get(0..2)` returns `A`, `é`, `中` correctly and the parent's
bytes are right (`004100e94e2d…`), so the STORE is fine and only `toString()` is
wrong. Heap-backed char views are correct. Not diagnosed further here; it is
listed so the count in §5 is accounted for.
