# WORKER-4-NOTE-6 — the NIO buffer and channel bodies: 170 cases, ZERO diffs, and 47 of 100 shadows actually reached

**Status: MEASURED. No source change — two probes and a coverage number.**
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

## 6. `W4Direct` — the gap this note nominated, closed in the same session

§4 named `java/nio/DirectByteBuffer` as the sharpest of the eight unreached
columns: `W4Nio` allocates a direct buffer and checks its SHAPE — `isDirect`,
`hasArray`, the `array()` refusal — and does almost nothing THROUGH it, and
direct buffers are where the address arithmetic lives.

`regression-suite/probes/W4Direct.java` (added here) puts bytes through it.
**83 cases, ZERO diffs against the oracle in both modes**, and the control says
it landed where it was aimed:

```text
  java/nio/DirectByteBuffer rows FIRED : 19    (W4Nio reached NONE)
    get 47 · session 15 · getInt 5 · isDirect 3 · get([B) 2 · isReadOnly 2
    put 2 · getChar 1 · getDouble 1 · getFloat 1 …
```

Its shape is deliberate in two ways:

* **Every case runs against a HEAP buffer too**, through one shared `exercise`
  routine. A difference between the two backings therefore shows up as a diff
  between two lines of the SAME run, rather than needing a second oracle
  comparison to notice — which is the failure mode a direct-vs-heap divergence
  actually has.
* **It ends at a real `FileChannel`.** A direct buffer handed to a channel has
  its address passed to the OS, so a wrong offset surfaces as wrong BYTES ON
  DISK rather than as an exception. It writes a direct buffer, reads it back,
  writes a SLICED direct buffer (the offset case), and reads into a buffer with
  a non-zero position — then checks the file contents, not the return code.

## 7. Combined coverage, and what is still not covered

Union of the two probes' registry dumps:

| | |
|---|---:|
| buffer/channel §1.4 shadows | **100** |
| reached by `W4Nio` + `W4Direct` | **47** |
| still unreached | **53** |

`DirectByteBuffer` is now at zero unreached. What is left:

| class | unreached shadows |
|---|---:|
| `java/nio/ByteBuffer` | 15 |
| `java/nio/Buffer$2` | 10 |
| `java/nio/CharBuffer` | 6 |
| `java/nio/Buffer` | 5 |
| `java/nio/IntBuffer` | 5 |
| `java/nio/LongBuffer` | 5 |
| `java/nio/ByteBufferAsCharBufferB` | 4 |
| `java/nio/HeapByteBuffer` | 1 |

**170 cases and zero diffs still buys "47 of 100 agree", not "the family is
clean."** The honest way to read the pair is: every question anyone has thought
to ask of these bodies is answered correctly, and slightly under half the rows
have been asked anything at all.

The residue is mostly the TYPED VIEW classes (`Buffer$2`, `CharBuffer`,
`IntBuffer`, `LongBuffer`, `ByteBufferAsCharBufferB` — 30 of the 53), which both
probes touch only at the edges: `asIntBuffer`/`asCharBuffer`/`asLongBuffer` get
one write and one read each. A third probe driving the view classes at every
width, with both backings and both byte orders, would take the covered fraction
past two thirds; that is the next one worth writing and this note does not
write it.

## 8. Nomination

**N1 — CLOSED by §6, in the same session it was raised.** `W4Direct` reaches 19
`DirectByteBuffer` rows and finds no divergence.

**N2 — the typed VIEW classes are 30 of the 53 rows still unreached.** §7. A
probe driving `asIntBuffer` / `asCharBuffer` / `asLongBuffer` / `asShortBuffer`
/ `asFloatBuffer` / `asDoubleBuffer` at every width, over BOTH backings and BOTH
byte orders, with write-through checked back through the parent's bytes. The
view classes are the ones whose names encode the backing and the order
(`ByteBufferAsCharBufferB`), which is exactly the kind of family where one arm
drifts.
