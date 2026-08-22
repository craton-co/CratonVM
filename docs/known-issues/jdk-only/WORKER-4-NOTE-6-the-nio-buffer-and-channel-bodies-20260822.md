# WORKER-4-NOTE-6 — the NIO buffer and channel bodies: 87 cases, ZERO diffs, and 27 of 95 shadows actually reached

**Status: MEASURED. No source change — a probe and a coverage number.**
2026-08-22, Linux (Azure host 2), Temurin 25.0.4+7, on the handoff tip
`8104bfd2f` built unmodified.

## 1. What had not been asked

`WORKER-4-1` … `-4-3` probed the `java.io` census classes and the NIO
RECEIVERS (`W4Abstract`, 63 of them, now 0 abstract). **None of them asked
whether the buffer and channel BODIES answer correctly.**
`native-io/src/direct_buffer.rs`, `nio_native.rs` and the `FileChannel` family
are large surfaces whose only gate is `RJdkNio` — and a corpus vector asks the
questions its author thought of.

`regression-suite/probes/W4Nio.java` (added here) asks 87 of them.

## 2. The result

**87 cases, ZERO diffs against the oracle, in both modes.**

Covered: allocation and initial state for heap / direct / wrapped / range-wrapped
buffers; `hasArray` / `arrayOffset` / `array()` including the `UnsupportedOperation`
on a direct buffer; relative put/get for byte/short/int/long with `pos/lim/cap`
printed at every step; absolute accessors NOT moving the cursor, and their
bounds and negative-index refusals; byte order both ways, the native-order
check, and the JDK quirk that **`slice()` resets to `BIG_ENDIAN`** regardless of
the parent's order; slice and duplicate sharing the STORE but not the cursor,
verified by writing through the slice and reading through the parent;
`compact` / `mark` / `reset` / `clear` and `reset` with no mark; read-only views
and their two refusals; `asIntBuffer` / `asCharBuffer` write-through;
`CharBuffer.wrap`; charset encode/decode through buffers and a strict decoder on
malformed input; and for `FileChannel` — `size`, relative and ABSOLUTE `read`
(and that the absolute form leaves the position alone), `position` set/get,
`write`, `truncate` and its effect on position, `force`, `transferFrom`,
`transferTo`, the three closed-channel refusals, and the read-into-read-only and
write-to-read-only refusal TYPES.

## 3. The control, because a green arm is evidence about the question it asked

A probe that never reaches a native measures the JDK, not this VM.
`--dump-native-registry` on the same runs, identical in both modes:

```text
  owned natives in the ByteBuffer / Buffer / FileChannel family : 222
  FIRED during W4Nio                                            :  39
  total invocations                                             : 387
    ByteBuffer.hasRemaining 67 · FileChannel.isOpen 50 · ByteBuffer.position 37
    ByteBuffer.limit 36 · capacity 27 · Buffer.<init> 21 · allocate 16 …
```

So the zero is a measurement of CratonVM's own bodies.

## 4. What it does NOT cover, stated as a number

Of those 222 owned rows, **95 are §1.4 shadows** (the real method is declared,
has `Code`, and is not `ACC_NATIVE`). `W4Nio` reaches **27** of them and leaves
**68** unreached:

| class | unreached shadows |
|---|---:|
| `java/nio/ByteBuffer` | 18 |
| `java/nio/DirectByteBuffer` | 18 |
| `java/nio/Buffer$2` | 10 |
| `java/nio/CharBuffer` | 6 |
| `java/nio/Buffer` | 5 |
| `java/nio/IntBuffer` | 5 |
| `java/nio/ByteBufferAsCharBufferB` | 4 |
| `java/nio/HeapByteBuffer` | 1 |

**The claim is "27 of 95 buffer/channel shadows agree with the oracle", not
"the family is clean."** `[a narrow probe reports its own reach, not the
defect]`. The `DirectByteBuffer` column is the most interesting gap: this probe
allocates a direct buffer and checks its shape, but does almost nothing
THROUGH it, and direct buffers are where the address arithmetic lives.

## 5. Why the zero is still worth having

The same reasoning as `WORKER-4-2` §6.1's `PrintStream` result. 27 shadow rows
answering identically to HotSpot on everything asked does not make them
contract-compliant — they still stand in front of real `java.base` bytecode —
but it means retiring THOSE is a pure §1.4 exercise with no behaviour to
preserve, which is the cheapest kind to schedule. Before this, nobody knew
which of the 95 were in that position, and "we do not know" is what stops a
retirement.

## 6. Nomination

**N1 — a `W4Direct` probe for the 18 unreached `DirectByteBuffer` shadows.**
Put bytes THROUGH a direct buffer rather than at it: relative and absolute
accessors at every width, `slice`/`duplicate`/`asReadOnlyBuffer` of a direct
buffer, `order()` on the views, a direct buffer handed to `FileChannel.read` /
`write` / `transferTo`, and `Buffer.address` visibility via a channel round
trip. That is where address arithmetic errors live and where this probe
deliberately stopped.
