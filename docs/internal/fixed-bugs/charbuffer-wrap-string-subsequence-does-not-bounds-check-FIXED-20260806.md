# `CharBuffer.wrap(String).subSequence(start, end)` accepted any range instead of throwing

**Status:** FIXED 2026-08-06. Retired from `docs/known-issues/`.

**Reproducer, and now the regression measurement:** `probes/CharBufferWrapProbe`
— 61 rows, run against HotSpot 25 and against CratonVM and diffed.
**44 divergences → 1**, the survivor being a distinct pre-existing defect filed
separately (see *What is deliberately NOT fixed here*).

```
                                             HotSpot 25                         CratonVM before
CharBuffer.wrap(str).subSequence(-1,2)  IndexOutOfBoundsException | null    pos=-1 lim=2
CharBuffer.wrap(str).subSequence(0,99)  IndexOutOfBoundsException | null    "Hello, World" + 87 NULs
```

`str` is `"Hello, World"` (length 12). The second row is the one that mattered:
the call read 87 code units past the end of the wrapped sequence and handed
them back as content.

## The filed record named the wrong mechanism

It said:

> `subSequence` has no native, so the real bytecode runs against a receiver
> whose `limit` it cannot see, the range check passes for anything.

Both halves were wrong, and a five-minute `--dump-native-registry` would have
said so.

1. **`subSequence(II)` *is* a native** (`register_p62_char_buffer`,
   `invocations=17` on the probe run). It simply did no bounds checking at all:
   it took `start`/`end` verbatim, computed `pos + start` / `pos + end`, and
   set the result's `capacity` to its own limit.
2. **The `limit` was perfectly readable.** `cb_write_hb` writes `position`,
   `limit` and `capacity` by name *and* by index. Nothing was hidden from
   anything.

The record was written from reading code and reasoning about it. The registry
dump and a `getClass()` row would have replaced both guesses with facts, and
the fact they pointed at was better than either guess.

## What was actually wrong: the stamped class

`CharBuffer.wrap(char[])` and `CharBuffer.wrap(CharSequence)` were natives that
returned an object stamped with the **abstract** `java/nio/CharBuffer`. That one
choice caused every symptom:

* every `CharBuffer` method CratonVM does not implement natively had no
  bytecode to fall back to — `slice()`, `duplicate()` and `asReadOnlyBuffer()`
  raised `AbstractMethodError`;
* `subSequence` fell to the unchecked native above;
* `get(int)` raised `IllegalArgumentException("index: 99")` — not even in the
  `IndexOutOfBoundsException` hierarchy;
* `wrap(String)` reported `hasArray()==true`, `isReadOnly()==false` and allowed
  `put`, where the JDK's `StringCharBuffer` is read-only;
* `wrap(char[])` **copied** the array, so writes through the buffer never
  reached the caller's array as the JDK specifies.

Two things in the same file already said what the answer was. `allocate` stamps
the concrete `java/nio/HeapCharBuffer`, with a comment — *"Use HeapCharBuffer
(concrete) not CharBuffer (abstract) so real-JDK bytecode methods like
`compact()` dispatch correctly"* — and is correct on every one of those shapes.
And the **three-argument** `wrap(CharSequence, int, int)` was never intercepted
at all, so it already built a real `StringCharBuffer` and already matched
HotSpot exactly, `capacity` and null message included.

## A concurrent session found the same thing and stopped one step earlier

While this was in progress, `dev` landed `86090abee` — *"CharBuffer.wrap
stamped the abstract class, so slice() had no body"* — from a different repro
(`CBSLICE.java`, an `AbstractMethodError` out of `wrap(a).slice()`). It reached
the same diagnosis and changed the stamp to the concrete
`java/nio/HeapCharBuffer`.

That closes the `AbstractMethodError` half and, because
`HeapCharBuffer.subSequence` bounds-checks in bytecode, the out-of-range read
this record is about. The merge keeps the **stronger** resolution — not
registering the natives at all in real-JDK mode — because stamping a concrete
class still answers `wrap(String)` with a `HeapCharBuffer` over a *copy* of the
sequence, so `getClass()`, `isReadOnly()`, `hasArray()` and `put` all stay
wrong, and `subSequence`'s message stays `HeapCharBuffer`'s rather than
`StringCharBuffer`'s. Synthetic-JDK mode, which keeps the natives, takes
`dev`'s concrete stamp.

The other commit in that range, `e02262b15` (the `mark` write clobbering
`Buffer.address`), is untouched and preserved.

## The fix

**Delete the two two-argument `wrap` natives in real-JDK mode.** Their bytecode
is `wrap(array, 0, array.length)` / `wrap(csq, 0, csq.length())` — the
three-argument forms that already worked. Everything above comes right at once,
from the JDK's own code, with no bounds logic to reimplement or keep in sync.
Synthetic-JDK mode keeps them: there is no `CharBuffer` bytecode there to fall
back to.

Four supporting changes, each of which the deletion exposed or required:

* **`CharSequence`, not `String`.** `StringCharBuffer` holds the wrapped
  sequence in `str`, and CratonVM's natives read it with
  `NativeContext::read_string`, which only understands a real
  `java.lang.String`. A `StringBuilder` or an application `CharSequence` read
  back **empty** — so `encode(CharBuffer.wrap(charChunk))` produced zero bytes
  and reported success, which is exactly the 144-failure Tomcat
  `MessageBytes.toBytes` shape the deleted native was written to prevent.
  `charset_buffers::read_wrapped_char_sequence` does the `read_string`-then-
  virtual-`toString()` fallback the deleted native used to do at construction
  time, at the three places the sequence is actually read.
  **`Charset.encode(CharBuffer.wrap(csq))` returning an empty `ByteBuffer` was
  pre-existing for the three-argument `wrap` — the deletion did not cause it,
  it made it reachable from more callers, and it is fixed here.**
* **The `subSequence` native keeps its bounds check anyway.** It is still
  reachable for receivers genuinely stamped abstract — `ByteBufferAsCharBuffer
  .subSequence` builds one. It now performs
  `Objects.checkFromToIndex(start, end, limit() - position())` and propagates
  the parent's `capacity` instead of its own limit.
* **`ByteBufferAsCharBuffer.subSequence` had the identical hole**, and said so
  in its own code: it read `lim` and then `let _ = lim;`. `asCharBuffer()
  .subSequence(0, 99)` on a six-char view decoded 93 code units from past the
  end of the underlying `byte[]`.
* **`Buffer.position(int)` / `limit(int)` now validate.** The JDK throws
  `IllegalArgumentException` (`newPosition > limit`, `newLimit > capacity`,
  and the two distinct negative cases), and these natives wrote the field
  unchecked — and wrote the *indexed* slot only, which on a real
  `StringCharBuffer` is not `position` at all.

## Verification

`probes/CharBufferWrapProbe`, 61 rows, class and message and the buffer's own
`position`/`limit`/`capacity`/`remaining`/`length`, diffed against HotSpot 25:

| | before | after |
|---|---:|---:|
| divergences | 44 | **1** |

Also re-measured, unchanged:

* `probes/PreconditionsFormatterProbe` — the two
  `CharBuffer.wrap(str).subSequence(...)` rows this record is about now match,
  taking it from 4 divergences of 58 to **2**, both of them the separately
  filed `array-index-out-of-bounds-has-no-detail-message`.
* `probes/ByteBufferBulkProbe` — checksum still `7040159201000546794`,
  byte-identical to HotSpot.
* `probes/StringPolicyMatrixProbe` — still 3 divergences of 392.

All of the above re-run on the **merged** state (see the section above — two
independent fixes for one defect landed in the same window, and neither had
been measured against the other), with identical results.

Suites on the merged state, 0 failures: `native-builtins --lib` 3287,
`vm --lib` 2425, `native-io --lib` 402, `types --lib` 497,
`vm --test wp8_10_9_string_contains_native` 6, and the blocking synthetic-JDK
VM gate.

## What was deliberately NOT fixed here — and was fixed the next day

One row: `CharBuffer.wrap(str).subSequence(3, 2)` returned an empty buffer
(`pos=3 lim=2 rem=0`) where HotSpot throws `IndexOutOfBoundsException`.

`StringCharBuffer.subSequence` does not range-check that pair itself — it lets
the `Buffer` constructor's `position(pos)` raise `IllegalArgumentException` and
catches it. **CratonVM's `Buffer.position(int)` validation is correct and does
fire** — `IntBuffer.allocate(5).position(9)`, `LongBuffer`, `DoubleBuffer`,
`ShortBuffer.limit(9)` and `CharBuffer.allocate(5).limit(3).position(4)` all
throw the right exception with HotSpot's exact message — but it did not fire
when called from inside `Buffer.<init>`. That was pre-existing: the
never-intercepted three-argument `wrap(str, 0, 5).subSequence(3, 2)` did not
throw on the pre-change binary either.

**FIXED 2026-08-06** →
[`buffer-constructor-does-not-validate-position-and-limit-FIXED-20260806.md`](buffer-constructor-does-not-validate-position-and-limit-FIXED-20260806.md),
which takes `probes/CharBufferWrapProbe` from this record's 1 divergence to
**0 of 61**. The mechanism this record guessed at — covariant-bridge dispatch
inside a constructor — was wrong again: `java/nio/Buffer.<init>` is itself a
registered native, and it wrote the fields without performing any of the
constructor's checks. Nothing was dispatched anywhere surprising.

## What this one is worth keeping

**A known-issues record's stated mechanism is a hypothesis, and this one was
written by the same person who then fixed it.** Both of its claims — "there is
no native" and "the layout is unreadable" — were false, and one
`--dump-native-registry` plus one `getClass()` row disproved both. The
`invocations` column in that dump is the cheapest available answer to "does
this code actually run?", and it is worth spending before believing any
description of a dispatch path, including your own from last week.

**The class you stamp on a synthetic object decides how much of the JDK you
have to reimplement.** Stamping the abstract base cost, in this one case: a
missing bounds check, three `AbstractMethodError`s, a wrong exception class, a
lost read-only flag, and a copied-instead-of-aliased array. Stamping the
concrete class — or better, not intercepting the factory at all — costs
nothing and inherits all of it. `allocate` had the right answer and a comment
explaining it, twenty lines above the two natives that did not.
