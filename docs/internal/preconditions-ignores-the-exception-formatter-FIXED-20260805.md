# `Preconditions.checkFromToIndex` ignored its exception formatter and always threw `ArrayIndexOutOfBoundsException`

**Status:** FIXED 2026-08-05. Retired from `docs/known-issues/`.

**Supersedes** `string-substring-bounds-throw-arrayindexoutofbounds.md`, filed
2026-08-04, **which was wrong about the cause and wrong about the blame.** That
record said `String.substring` throwing `ArrayIndexOutOfBoundsException` was a
pre-existing defect that removing the forced-native `String` policy had merely
*surfaced*. It was not pre-existing: the same change **deleted the fix for it**.
See *How the earlier record got it wrong* — the mistake is worth keeping,
because it is a repeatable one.

**Reproducer, and now the regression measurement:**
`probes/PreconditionsFormatterProbe`, 50 rows, run against HotSpot 25 and
against CratonVM and diffed.

## What was wrong

`jdk/internal/util/Preconditions`' four-argument overloads were overridden in
`native-builtins/src/lib.rs` (~11130). The `BiFunction` is the JDK's *exception
formatter* — the whole reason the four-argument overload exists — and the
override ignored it, then threw `ArrayIndexOutOfBoundsException`
unconditionally:

```rust
if from < 0 || from > to || to > length {
    Err(RuntimeError::ArrayIndexOutOfBoundsException { index: from }.into())
} else {
    Ok(Some(Value::Int(from)))
}
```

Two things were wrong with that, in order of severity:

1. **The formatter was discarded.** `String` passes
   `Preconditions.SIOOBE_FORMATTER`, whose entire job is to make the result a
   `StringIndexOutOfBoundsException`. Every `String`-domain caller —
   `substring`, `indexOf(I,I,I)`, the `byte[]` / `char[]` constructors,
   `getChars`, `getBytes` — is entitled to that class.
2. **The fallback class was wrong even with no formatter.** When `oobef` is
   `null` the real `Preconditions.outOfBounds` throws
   `IndexOutOfBoundsException`, never `ArrayIndexOutOfBoundsException`. So the
   non-`String` callers (NIO buffer slicing, via `Objects.checkFromToIndex`)
   got a wrong class too — and a *subclass* of the right one, which is the
   direction that breaks a `catch`.

`StringIndexOutOfBoundsException` and `ArrayIndexOutOfBoundsException` are
siblings under `IndexOutOfBoundsException`. `catch (IndexOutOfBoundsException)`
is unaffected; **`catch (StringIndexOutOfBoundsException)` is not**, and real
parsing and validation code writes exactly that. This was a control-flow
defect, not a message defect.

## Narrowed first, on 2026-08-05, by a bypass — and the class depended on the SIGN

Between the filing and this fix, a concurrent lane narrowed the record and
found the half `substring` was hiding. Worth keeping, because the *method* is
the transferable part: the `StringPolicyMatrixProbe` rows for `charAt` only
ever showed a null MESSAGE, which hid a wrong CLASS. A five-line probe against
a HotSpot 25 control (`"hello world!"`, length 12):

```text
                 HotSpot                             CratonVM before
  charAt(-1)     SIOOBE "Index -1 out of bounds..."  ArrayIndexOutOfBoundsException, msg=null
  charAt(12)     SIOOBE "Index 12 out of bounds..."  SIOOBE, msg=null
```

**The exception class depended on the sign of the index.** A negative index
reached `Preconditions.checkIndex` (which discarded the formatter and threw
AIOOBE); an index past the end was caught earlier and produced SIOOBE. So
`catch (StringIndexOutOfBoundsException)` around `charAt` worked for one
out-of-range direction and not the other. **A row can differ on message text
and be hiding a class difference underneath**, and a diff that already counts
it as "diverging" cannot report that it got worse.

That lane added `java/lang/String.checkIndex(II)V` as F4's third member and
moved every `StringIndexOutOfBoundsException` construction site to
`RuntimeError::sioobe_index` / `sioobe_range` / `sioobe_range_size`, which
build HotSpot's three message shapes verbatim in one place. Measured:
`StringPolicyMatrixProbe` 21 → 8 divergences, 13 fixed, 0 regressed, identical
under default/`--nojit`/`--jdk-only`.

It named exactly what it had not done: the `BiFunction` was still never
invoked, and the non-`String` callers still got the wrong class, which needed
an `IndexOutOfBoundsException` variant `RuntimeError` did not have. Both are
what this record closes. The `sioobe_*` constructors survive and are now the
single source of the three message shapes — `native-builtins/src/preconditions.rs`
formats through the same module rather than keeping a second copy.

All three bypasses are retired, `checkIndex` included.

## What changed

### 1. The natives honour the formatter (`native-builtins/src/preconditions.rs`)

A new module replaces the six inline closures. The success path is unchanged —
it still does the bounds arithmetic in Rust and allocates nothing, which is why
these are natives at all: `String.charAt` reaches
`Preconditions.checkIndex(index, length, SIOOBE_FORMATTER)` on **every
character read**, and handing that to the interpreter costs a Java frame per
`charAt`.

The failure path resolves the exception class in three steps, cheapest first:

1. `oobef == null` → `IndexOutOfBoundsException`, the JDK's own fallback.
2. `oobef` is reference-identical to one of `Preconditions`' three static
   formatters (`SIOOBE_FORMATTER` / `AIOOBE_FORMATTER` / `IOOBE_FORMATTER`) →
   that formatter's class, built directly. This covers every caller inside
   `java.base` without running any Java code on the throw path.
3. anything else — an application-supplied formatter, and `java.nio.Buffer`
   declares one of its own — → invoke `oobef.apply(checkKind, args)` and throw
   whatever object comes back, exactly as `Preconditions.outOfBounds` does. A
   `null` return falls back to step 1, again per the JDK.

The message is `Preconditions.outOfBoundsMessage` reproduced verbatim, pinned
by unit tests against strings taken from HotSpot.

### 2. `Preconditions.<clinit>` is no longer suppressed

Step 2 needs the three static formatters to exist. The registration that made
`<clinit>` a no-op justified itself like this:

> The only state `Preconditions.<clinit>` creates is the set of `BiFunction`
> exception formatters, built with invokedynamic + LambdaMetafactory. […] this
> class is initialized from `String.charAt` at the very bottom of bootstrap —
> before `java.lang.invoke` is usable — so running the real `<clinit>` there
> would be a bootstrap-ordering hazard.

**The premise stopped being true.** The JDK rewrote those formatters as
anonymous inner classes for precisely that reason; the source even says so
("it's not feasible in practice, because `Preconditions` is used in many
fundamental classes such as `String`, lambda expressions […] could lead to
recursive calls"). `javap -c jdk.internal.util.Preconditions` on JDK 25 shows
three `new`/`invokespecial`/`invokestatic` triples and no `invokedynamic`
anywhere. A comment that states a fact about the JDK has a shelf life; this one
had outlived it by several releases and was still being trusted.

### 3. `RuntimeError::IndexOutOfBoundsException` now exists

There was no way to express the class. That absence had already been worked
around **21 times** across `native-io/src/lib.rs` and
`native-builtins/src/servlet.rs` as

```rust
RuntimeError::IllegalArgumentException { message: "IndexOutOfBoundsException".to_string() }
```

— an `IllegalArgumentException` that is not in the `IndexOutOfBoundsException`
hierarchy at all, so `catch (IndexOutOfBoundsException)` around a buffer access
never saw it. All 21 now raise the real class.

### 4. F4 is retired — all three of it — which is the test that the fix worked

`java/lang/String.checkBoundsBeginEnd(III)V`, `checkBoundsOffCount(III)I` and
`checkIndex(II)V`
were intercepted with SIOOBE-correct natives to route `String`-domain callers
around the broken override. They are gone. Their bytecode reaches
`Preconditions` with `SIOOBE_FORMATTER` and now gets the right class *and* the
right message, which the natives never produced (they threw a message-less
SIOOBE). `vm/tests/wp8_10_9_string_contains_native.rs`'s two-sided pin was
updated; `native-builtins/src/lang_string.rs` gained the inverse test, so
re-adding an interceptor there fails rather than passing silently.

### 5. The NIO shim, which is what the verification section asked for

The old record asked for "a non-`String` case (an NIO buffer slice) to cover
the second defect above, which F4 does not touch." Adding one found that
CratonVM's own `java.nio` natives shadow the bytecode that would have reached
`Preconditions`, and were worse than the defect being fixed:

* **Every absolute accessor on `java/nio/ByteBuffer` skipped its bounds check
  entirely.** `ByteBuffer.allocate(8).get(-1)` returned `0`;
  `put(8, b)` was dropped on the floor. No exception anywhere — a silent wrong
  answer, and a silent out-of-range write. Twelve accessors
  (`get`/`put`/`getShort`/`putShort`/`getChar`/`putChar`/`getInt`/`putInt`/
  `getLong`/`putLong`/`getFloat` …) now go through one
  `Buffer.checkIndex(i, nb)` helper.
* `slice(index, length)` and the bulk `get`/`put(byte[], off, len)` raised
  `ArrayIndexOutOfBoundsException` with a comment arguing it was fine "because
  it is a subclass and still satisfies `catch (IndexOutOfBoundsException)`".
  That is defect 2's reasoning, restated: a subclass satisfies the wide catch
  and breaks every narrower one.
* `CharBuffer.charAt` raised `IllegalArgumentException("charAt index: N")`.

Note the two different message contracts, both now honoured: `Objects.check*`
and `slice(index, length)` carry `outOfBoundsMessage` text, while every
*absolute* `Buffer` accessor goes through `Buffer`'s own formatter, whose body
is `new IndexOutOfBoundsException()` — `getMessage()` is null there.

This closed all three items of
[`bytebuffer-jdk-contract-divergences`](bytebuffer-jdk-contract-divergences-FIXED-20260805.md),
including the one that record had classified as "**Not a defect** — AIOOBE *is*
a subclass of IOOBE, so every `catch (IndexOutOfBoundsException)` caller still
matches." `probes/ByteBufferBulkProbe`'s checksum — which covers every byte
produced and every exception type thrown — went from `2662755913314845620` to
`7040159201000546794`, HotSpot's.

### 6. `String`/`StringBuilder` bounds natives carry their message

`String.charAt`, `String.codePointAt`, `AbstractStringBuilder.charAt` and
`AbstractStringBuilder.codePointAt` had the right class with a null message.
`charAt` showed the interesting shape: the **first** out-of-range call at a
call site produced HotSpot's message and every later one produced `null`,
because the interpreter's inline cache fills on first use and then dispatches
the intrinsic table entry (`native-builtins/src/intrinsics/mod.rs`), which
routes to the native. A per-call-site behaviour change is easy to misread as
nondeterminism.

`AbstractStringBuilder.substring(int)` / `substring(int, int)` were **clamping**
out-of-range arguments instead of throwing: `sb.substring(3, 2)` returned `""`
and `sb.substring(-1)` returned the whole sequence, where the real body opens
with `Preconditions.checkFromToIndex(start, end, count, SIOOBE_FORMATTER)`.

## Verification

`probes/PreconditionsFormatterProbe` prints class **and** message for 50 rows
across four domains and is diffed against HotSpot 25.

| | before | after |
|---|---|---|
| rows identical to HotSpot | — | **46 / 50** |
| the eight rows the old record named | 0/8 | **8/8** |
| `Objects.check*` (null formatter) | 0/8 | **8/8** |
| NIO buffer rows | 6/18 | **16/18** |

The eight rows the old record named as the acceptance test —
`substring1(PLAIN,-1)`, `substring1(PLAIN,len+1)`, `substring2(PLAIN,3,2)`,
`substring2(PLAIN,-1,3)`, `substring2(PLAIN,0,len+1)`,
`substring2(PLAIN,MIN,MAX)`, `new String(utf8,-1,2,"UTF-8")` and
`new String(utf8,0,999,"UTF-8")` — match HotSpot exactly on both halves.

Also green: `cargo test -p cratonvm-native-builtins --lib` (3273),
`-p cratonvm-native-io --lib` (380), `-p cratonvm-types --lib` (492),
`-p cratonvm-vm --test wp8_10_9_string_contains_native` (6).

## What is deliberately NOT fixed here

Four rows still diverge. Both are separate defects that this probe found rather
than residues of this one, and both are filed:

* `CharBuffer.wrap(String).subSequence(-1, 2)` does not throw at all — it
  returns an empty/oversized buffer.
  `CharBuffer.wrap(CharSequence)` builds a *synthetic* 5-field
  `java/nio/CharBuffer` rather than a `StringCharBuffer`, so the real
  `subSequence` bytecode (which would reach `Buffer.checkIndex` and therefore
  `Preconditions`) never sees a usable `limit`. That is a CharBuffer-layout
  gap, not a formatter gap →
  [`charbuffer-wrap-string-subsequence-does-not-bounds-check.md`](../known-issues/charbuffer-wrap-string-subsequence-does-not-bounds-check.md).
* An out-of-range array load and `System.arraycopy` produce
  `ArrayIndexOutOfBoundsException` with a null message where HotSpot has
  `"Index 9 out of bounds for length 4"`. `RuntimeError::ArrayIndexOutOfBoundsException`
  carries only an index, never the length, and it is constructed at ~40 sites
  across five crates. The **class** is right everywhere, so nothing catches
  differently →
  [`array-index-out-of-bounds-has-no-detail-message.md`](../known-issues/array-index-out-of-bounds-has-no-detail-message.md).

## How the earlier record got it wrong

It reasoned from a control: `charAt(-1)` still produced
`StringIndexOutOfBoundsException` while `substring(-1)` produced
`ArrayIndexOutOfBoundsException`, therefore "CratonVM can throw the right
class, so this is `substring`'s bounds check specifically" — and concluded
pre-existing. The control was sound and the conclusion did not follow: it
established *where* the difference was, not *when* it appeared. Nothing checked
whether the change under test had removed something.

What would have caught it in one step: **grep the registry for the triple
before blaming the bytecode.** `String.checkBoundsBeginEnd` was a registered
native carrying a 40-line comment describing this exact failure mode, and the
change deleted it as part of a category-wide sweep. A sweep that drops
registrations by category has to derive its exemptions **from the registrations
it is dropping**, not from the ones anybody remembered to look for.

## What this one is worth keeping

Three shapes, all of which cost time here:

1. **A comment that states a fact about a dependency has a shelf life.** The
   `<clinit>` suppression was correct when it was written and had been false
   for several JDK releases. It was load-bearing for the wrong class the whole
   time, and nothing re-checked it because the comment read as authoritative.
2. **"It's a subclass, so the catch still matches" is not a defence.** It
   appeared verbatim at three different sites here, each written independently.
   A subclass satisfies the widest catch and breaks every narrower one, plus
   `instanceof` and `getClass()`. Where the JDK names a class, name that class.
3. **A missing `RuntimeError` variant becomes 21 workarounds, not one.** The
   absence of `IndexOutOfBoundsException` was routed around with an
   `IllegalArgumentException` carrying the wanted class's *name as a string*,
   at 21 sites, none of which was catchable as what the caller expected.
   Adding the variant was three lines.
