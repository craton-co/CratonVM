# The `java.io` Reader/Writer chain parks VM-internal values on JDK fields

**Status:** the `Files.newBufferedWriter` half is **FIXED 2026-08-05, by
deletion**. The three synthetic-only siblings remain OPEN — see
"The other three" below. Found by tracing the one *live* access site
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

## What it turned out to be, and what was done

The plan above was a side table keyed by the object, plus a name-based
predicate. Measuring the flagged arm before writing it changed the answer.

**The fd write was already default-OFF.** `open_buffered_writer` was registered
only under `CRATONVM_SYNTHETIC_BUFFERED_WRITER=1`; since 2026-06-18 the default
has run the real `BufferedWriter(OutputStreamWriter(Files.newOutputStream(p)))`
bytecode. So the 402 slot-0 reads per run in the original census were the
*discriminator*, not the write.

**The flagged arm had no working configuration left.**
`probes/BufferedWriterDiscriminatorProbe` under
`CRATONVM_SYNTHETIC_BUFFERED_WRITER=1` produced **zero bytes** for every one of
its five `Files.newBufferedWriter` writes, against a default arm that is
byte-identical to HotSpot on all thirteen lines. The overlay trace shows why,
and it is not the layout: the fd is written once
(`set_field slot=0 value=Int(3)`) and **every subsequent read of that slot on
the same object returns `Object(None)`** — destroyed before its first use.

So the path was **deleted**, along with the flag: it was the only writer of this
overlay in a default build, it had been documented as data-losing for six weeks,
and relocating a dead path's fd to a side table would have been polish on
something nobody can run.

**The discriminator's premise was separately false.** `bw_delegate_out` asked
"does raw slot 0 hold an `Int`?" under a doc comment claiming that slot holds
"the `lock`/`out` Writer set by the JDK constructor". No JDK lays it out that
way — `java.io.Writer` declares `writeBuffer` first and `lock` second, so slot 0
is the `char[]` and `out` is slot 2. It answered correctly anyway for an
unrelated reason: bytecode `new` writes an explicit `Object(None)` into every
reference slot, because an all-zero slot decodes as `Int(0)` and **not** as null
(the R-niche rule in `gc/src/gen_heap.rs`). Any BufferedWriter arriving from an
allocator that skips those defaults — `alloc_object` without descriptors, which
is what `alloc_concurrent_synthetic` uses — would have read `Int(0)` and been
classified as fd-backed **on fd 0**. That read is now
`#[cfg(feature = "synthetic-jdk")]`-gated, so the default build never asks a
JDK-owned slot who owns the object.

`java/io/BufferedWriter` slot 0 keeps its `_vm0` model row even so, because
`native_bw_init` in the `synthetic-jdk` build still parks an fd there. That is
the same standing-hazard reading the three siblings below carry.

The model itself was corrected on 2026-08-05 — `out` now sits at index 2, where
the real class declares it — and slot 0 was first left **anonymous** rather than
named `writeBuffer`, so the overlay would not be papered over by a name.

That turned out to be the wrong half-measure: an anonymous `_fN` over a real
*reference* is exactly the case the shadow-layout diff documents as
unfalsifiable, so the slot went from reporting a wrong NAME to reporting
**nothing at all**, and the census read as if the class were clean. Slot 0 is now
spelled `_vm0`, a third model spelling whose whole job is to keep a kind-3
overlay countable: `SlotVerdict::VmInternal`, tag `VM`, reported whenever a real
field exists at that index and silent when the slot is anchored past the real
layout (which is the shape a fix should reach).

## The other three, found the same way

Widening the model from "name the field" to "say when there is no field to name"
brought three more classes of the same family into the census. All of their
writers are `#[cfg(feature = "synthetic-jdk")]`, so unlike `newBufferedWriter`
these do not fire in the default build — but the models are shared, and the
`_vmN` rows are what will say so if a registration is ever ungated.

| class | slot | VM parks | real field there |
|---|---|---|---|
| `java/io/BufferedWriter` | 0 | fd `Int` (`Files.newBufferedWriter`) | `Writer.writeBuffer` `[C` |
| `java/io/BufferedReader` | 0 | fd `Int` (`native_br_init`) | `Reader.lock` `Object` |
| `java/io/InputStreamReader` | 0 | fd-or-`InputStream` (`native_isr_init`) | `Reader.lock` `Object` |
| `java/io/InputStreamReader` | 1 | the wrapped `InputStream` | `Reader.skipBuffer` `[C` |
| `java/io/OutputStreamWriter` | 0 | fd `Int` (`native_osw_init`) | `Writer.writeBuffer` `[C` |

`InputStreamReader` and `OutputStreamWriter` were the sharper finding of the two:
their models named slot 0 `in` / `out`, and **neither class declares such a
field at all** — a real `InputStreamReader` has one field, `sd`, and the wrapped
stream lives inside the `StreamDecoder`. There was no index to move the name to.

`Reader.lock` is the one to watch. It is **never null** on a real reader — the
`Reader(Object lock)` constructor sets it to the wrapped stream, confirmed
against Temurin 25.0.3 by `probes/ReaderWriterLayoutProbe.java` — and real
`Reader` bytecode does `synchronized (lock)`. An fd `Int` written there is not a
dormant wrong value; it is a monitor-enter on an integer.

`native_osw_write`, `native_osw_flush` and `native_osw_close` each read slot 0
expecting `Value::Int(fd)` and `return Ok(None)` when it is anything else, so on
a real layout they would be **silent no-ops** — a write that reports success and
produces no bytes. That is the failure mode to expect first if these
registrations are ever ungated.

## How to reproduce

```sh
CRATONVM_DBG=overlay,overlay-all,overlay-bt=BufferedWriter \
  cratonvm --real-jdk --java-home "$JAVA_HOME" -cp . JdkOnlyCensusLoadProbe 2>&1 \
  | grep -A 12 'class=java/io/BufferedWriter slot=0'
```

The Rust frame names `register_phase57_nio_file`'s closure; the Java frames do
not, which is the usual pattern for a defect that lives in native code.

For the whole family at once, the `VM` rows of the shadow-layout census:

```sh
CRATONVM_DBG=overlay,overlay-all cratonvm --real-jdk --java-home "$JAVA_HOME" -cp . JdkOnlyCensusLoadProbe 2>&1 | grep '\[OVERLAY-LAYOUT\].* VM '
```

`probes/ReaderWriterLayoutProbe.java` is the behavioural companion: it prints
paired properties for all four classes (including the KIND actually found in
`lock` / `writeBuffer` / `skipBuffer` at each stage) so a transcript can be
diffed byte-for-byte against HotSpot. It currently agrees with Temurin 25.0.3 on
every line, which is the point — the overlay is not yet observable from Java,
and this probe is what will notice when it becomes so.
