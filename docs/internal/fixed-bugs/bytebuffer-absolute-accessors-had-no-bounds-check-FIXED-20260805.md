# `ByteBuffer`'s absolute accessors had no bounds check at all — FIXED 2026-08-05

**Status:** FIXED.

**Reproducer:** `probes/NioBufferBoundsProbe`.

## What was wrong

`ByteBuffer.wrap(new byte[8]).get(99)` returned **0**. So did `get(1000)`,
`get(-1)` and `get(Integer.MIN_VALUE)`. `put(99, (byte) 1)` returned normally.
HotSpot throws `IndexOutOfBoundsException` for every one of them.

The typed accessors were worse than the byte one, because they fabricated data
rather than a zero:

```text
ByteBuffer.wrap({1,2,3,4,5,6,7,8}).getInt(6)
  HotSpot  : IndexOutOfBoundsException      (an int at 6 needs bytes 6..9)
  CratonVM : 117964800                      == 0x07080000
```

That is the two real bytes at indices 6 and 7 followed by **two zeroes that are
not in the buffer**. A silently wrong integer is worse than an exception,
because a computation consumes it and carries on.

**It is contained.** The probe writes `0x5A` through a neighbouring buffer and
re-checks it afterwards, and re-checks the backing array: both are untouched
after every out-of-range read and write. So this was never a memory-safety hole
— out-of-range reads see a zero-filled void and out-of-range writes are dropped
— it is a correctness hole.

`allocateDirect` was unaffected: `DirectByteBuffer` has its own natives and
those bounds-check correctly.

## Why the check that existed did not run

`native-io/src/lib.rs`'s `native_bb_get_abs` **does** bounds-check. It is not
what runs. `--dump-native-registry`:

```
java/nio/ByteBuffer  get  (I)B  inv=76  registered_by native-builtins/src/servlet.rs:4446
java/nio/ByteBuffer  put  (IB)…  inv=65  registered_by native-builtins/src/servlet.rs:4575
```

A servlet-specific implementation owns the global `ByteBuffer` absolute
accessors, and its body was:

```rust
r.register(bb, "get", "(I)B", |ctx, args| {
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(s2_bb_get_byte(ctx, this, idx) as i32)))
});
```

Reading the source of the module that looks responsible would have proved
nothing; the census named the owner. Same technique as
[[reference_dump_native_registry_finds_the_clobbering_duplicate]].

## Why it read a zero instead of crashing

`s2_bb_get_byte` already refuses to read out of range, and says so:

> a negative or out-of-range index reads back 0 (the JDK would throw
> `IndexOutOfBoundsException`; our synthetic path stays panic-free and returns a
> benign zero byte instead)

That is the right instinct in the wrong place. The helper returns `i8` and has
no way to raise a Java exception, so being defensive is all it *can* do. The
contract belongs one level up, at the registration sites, where
`MethodCallResult` can carry a throw.

The adjacent bulk `get([BII)` carries a comment describing an earlier fix for
this same family (`BUG [nb-servlet]`: unchecked `off`/`len`, negative length
reinterpreted as ~1.8e19). That sweep fixed the bulk accessors and missed the
absolute ones.

## The fix

One `s2_bb_check_index(ctx, buf, index, width)` helper, applied to all twelve
absolute accessors — `get`/`put`, and `getShort`/`putShort`/`getChar`/`putChar`/
`getInt`/`putInt`/`getLong`/`putLong`/`getFloat`.

Three details that are easy to get wrong:

* **`limit`, not `capacity`.** `ByteBuffer.get(int)` is
  `Objects.checkIndex(i, limit)`. A buffer whose limit has been pulled in must
  refuse an absolute read past it even though the storage is still there.
* **Width matters.** `getInt(6)` on an 8-byte buffer is out of bounds even
  though index 6 is in range; the check is `index + width <= limit`.
* **Widened arithmetic.** `index + width` is computed in `i64` so an index near
  `Integer.MAX_VALUE` cannot overflow into a passing value.

The helper's zero-return stays exactly where it was, as the panic guard it was
written to be.

## Verification

`probes/NioBufferBoundsProbe` against a HotSpot 25 control: every out-of-range
row now throws `IndexOutOfBoundsException` where it previously returned a value,
across `wrap`, `allocate`, `slice` and every typed accessor.

Two rows still differ, on the MESSAGE only and with the class correct:
`allocate(8).get(99)` reports `"Index 99 out of bounds for length 8"` where
HotSpot's is null. Those reach the real JDK bytecode, which calls
`Objects.checkIndex` with `Buffer`'s own exception formatter — and our
`Preconditions` override does not invoke the formatter. That is the known open
item in
[`preconditions-ignores-the-exception-formatter`](../known-issues/preconditions-ignores-the-exception-formatter.md),
surfacing exactly where that record says it would.
