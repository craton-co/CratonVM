# `Files.newBufferedWriter` parks a VM-internal fd in `Writer.writeBuffer`

**Status:** OPEN, filed 2026-08-05. Found by tracing the one *live* access site
in the `java/io/BufferedWriter` row of the L4 shadow-layout census
(`CRATONVM_DBG=overlay,overlay-all,overlay-bt=BufferedWriter`, 402 reads per
run). **Kind 3** in the taxonomy of
[fabricated-object-layouts-leak-into-native-code.md](fabricated-object-layouts-leak-into-native-code.md):
a VM-internal value with no real JDK field to live in.

## What happens

`Files.newBufferedWriter` (`native-builtins/src/phases_late/nio_file.rs`)
allocates a `java/io/BufferedWriter` and stores the OS file descriptor in it as
a raw integer:

```rust
let bw = alloc_concurrent_synthetic(ctx, "java/io/BufferedWriter", 3);
ctx.set_field(bw, 0, Value::Int(fd as i32));
```

In real-JDK mode that object has the **real** layout, where slot 0 is
`java.io.Writer.writeBuffer`, a `char[]`. So the fd is written into the field
the JDK lazily allocates for `Writer.write(String)`.

The natives registered on `java/io/BufferedWriter` then use slot 0 as a
*discriminator* — `bw_delegate_out` reads it and treats "not an `Int`" as "this
is a real BufferedWriter, forward to its wrapped `out`". That is why the census
shows 402 reads of slot 0 per run on real BufferedWriters.

## Why it has not broken yet

Two accidents, neither of them a guarantee:

1. A real `BufferedWriter`'s `writeBuffer` is **null until first use**, so the
   discriminator's "not an `Int`" test reads `Object(None)` and answers
   correctly. The moment real `Writer` bytecode allocates the buffer the slot
   holds a `char[]` — still not an `Int`, so the discriminator survives, but the
   VM has now written an fd into a buffer the JDK may then use.
2. Our own natives shadow `write`/`flush`/`close` on `BufferedWriter`, so the
   real `Writer.write(String)` path that touches `writeBuffer` is rarely
   reached.

Both are the same shape as the `VarHandle.vform` finding: it does not fault
*only* because our natives intercept every operation, and the whole direction of
`--jdk-only` is to stop intercepting.

## The fix shape

Not a slot renumber — there is nowhere correct in a real `BufferedWriter` to put
an fd. It wants the **side table keyed by the object** that `LoaderMeta`
(`native-builtins/src/classloader.rs`) and `vh_meta_put` already use, plus a
`bw_has_synthetic_layout`-style predicate asked by a field NAME the real class
declares and the stub does not (`cb`, `nChars` or `maxChars` all work), so the
discriminator stops depending on the *value* found in a slot the JDK owns.

The model itself was corrected on 2026-08-05 — `out` now sits at index 2, where
the real class declares it — and slot 0 was deliberately left **anonymous**
rather than named `writeBuffer`, precisely so this overlay stays visible as an
unresolved question rather than being papered over by a name.

## How to reproduce

```sh
CRATONVM_DBG=overlay,overlay-all,overlay-bt=BufferedWriter \
  cratonvm --real-jdk --java-home "$JAVA_HOME" -cp . JdkOnlyCensusLoadProbe 2>&1 \
  | grep -A 12 'class=java/io/BufferedWriter slot=0'
```

The Rust frame names `register_phase57_nio_file`'s closure; the Java frames do
not, which is the usual pattern for a defect that lives in native code.
