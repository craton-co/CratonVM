# `Preconditions.checkFromToIndex` ignores its exception formatter and always throws `ArrayIndexOutOfBoundsException`

**Status:** OPEN in mechanism, CLOSED in observable behaviour, 2026-08-05.
Every caller -- `String`-domain and NIO alike -- now gets HotSpot's exact
exception class and message. What is still literally true of the title is that
the `BiFunction` formatter is never *invoked*: the overrides reproduce what it
would have produced instead of calling it. See *Where this stands* at the end.

**Supersedes** `string-substring-bounds-throw-arrayindexoutofbounds.md`, filed
2026-08-04, **which was wrong about the cause and wrong about the blame.** That
record said `String.substring` throwing `ArrayIndexOutOfBoundsException` was a
pre-existing defect that removing the forced-native `String` policy had merely
*surfaced*. It was not pre-existing: the same change **deleted the fix for it**.
See *How the earlier record got it wrong* — the mistake is worth keeping,
because it is a repeatable one.

**Reproducer:** `probes/StringUtf16HashProbe`, last three lines.

## What is wrong

`jdk/internal/util/Preconditions.checkFromToIndex(int, int, int, BiFunction)` is
overridden in `native-builtins/src/lib.rs` (~11130). The `BiFunction` is the
JDK's *exception formatter* — the whole reason the four-argument overload
exists — and the override ignores it, then throws
`ArrayIndexOutOfBoundsException` unconditionally:

```rust
if from < 0 || from > to || to > length {
    Err(RuntimeError::ArrayIndexOutOfBoundsException { index: from }.into())
} else {
    Ok(Some(Value::Int(from)))
}
```

Two things are wrong with that, in order of severity:

1. **The formatter is discarded.** `String` passes
   `Preconditions.SIOOBE_FORMATTER`, whose entire job is to make the result a
   `StringIndexOutOfBoundsException`. Every `String`-domain caller —
   `substring`, `indexOf(I,I,I)`, the `byte[]` / `char[]` constructors,
   `getChars`, `getBytes` — is entitled to that class.
2. **The fallback class is wrong even with no formatter.** When `oobef` is
   `null` the real `Preconditions.outOfBounds` throws
   `IndexOutOfBoundsException`, never `ArrayIndexOutOfBoundsException`. So the
   non-`String` callers (NIO buffer slicing, via `Objects.checkFromToIndex`)
   get a wrong class too — and a *subclass* of the right one, which is the
   direction that breaks a `catch`.

`StringIndexOutOfBoundsException` and `ArrayIndexOutOfBoundsException` are
siblings under `IndexOutOfBoundsException`. `catch (IndexOutOfBoundsException)`
is unaffected; **`catch (StringIndexOutOfBoundsException)` is not**, and real
parsing and validation code writes exactly that. This is a control-flow defect,
not a message defect.

## The workaround that exists, and must not be deleted again

**F4** (`native-builtins/src/lang_string.rs`, the block above
`register_string_utf16_natives`) intercepts the two `String` helpers directly
with SIOOBE-correct natives:

```
java/lang/String.checkBoundsBeginEnd(III)V
java/lang/String.checkBoundsOffCount(III)I
```

That bypasses the whole `Preconditions` chain for every `String`-domain caller
while leaving NIO's callers on the unchanged (still wrong) generic path. It was
written for a BouncyCastle `PKCS12$Mappings` AIOOBE blocker.

Both are now registered with `register_with_kind(.., Intrinsic)`, and that is
**load-bearing**: every other `java/lang/String` `Bridge` is dropped in
real-JDK mode by `NativeMethodRegistry::register`, these two are `Bridge` by
the ambient category at their site, so the drop took them and F4 regressed
immediately. A comment at the registration site says so.

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

## Narrowed 2026-08-05: `checkIndex` joined F4, and the class depended on the SIGN

The record above says `substring` is the symptom. It is not the only one, and
the missing piece was found by probing rather than reading -- the
`StringPolicyMatrixProbe` rows only ever showed a null MESSAGE for `charAt`,
which hid a wrong CLASS. A five-line probe (`probes/` -> `OobProbe` shape,
`"hello world!"`, length 12) against a HotSpot 25 control:

```text
                 HotSpot                             CratonVM before
  charAt(-1)     SIOOBE "Index -1 out of bounds..."  ArrayIndexOutOfBoundsException, msg=null
  charAt(12)     SIOOBE "Index 12 out of bounds..."  SIOOBE, msg=null
```

**The exception class depended on the sign of the index.** A negative index
reached `Preconditions.checkIndex` (which discards the formatter and throws
AIOOBE); an index past the end was caught earlier and produced SIOOBE. So
`catch (StringIndexOutOfBoundsException)` around `charAt` worked for one
out-of-range direction and not the other -- a control-flow defect, and the kind
that a message-only diff cannot see.

`java/lang/String.checkIndex(II)V` is now a native
(`native_string_check_index`, `lang_string.rs`), registered
`register_with_kind(.., Intrinsic)` exactly like its two F4 siblings and for
exactly the same reason. `javap -p java.lang.String` confirms the JDK declares
`static void checkIndex(int, int)`, so the registration is reached rather than
dead.

Alongside it, all `StringIndexOutOfBoundsException` construction sites moved to
`RuntimeError::sioobe_index` / `sioobe_range` / `sioobe_range_size`, which build
HotSpot's three message shapes verbatim in one place. The variant carries an
`Option<String>` message now; it discarded its index entirely before, which is
why every SIOOBE this VM threw had `getMessage() == null`.

### Effect, measured

`StringPolicyMatrixProbe`: **21 -> 8 divergences**, the 13 fixed rows being
exactly the predicted ones (31-35 `charAt`, 83-88 `substring`, 292-293
`new String(byte[],int,int,Charset)`), **0 regressions**, and no still-divergent
row changed value. Identical in default (JIT), `--nojit` and `--jdk-only`.

### Item 2 closed the same day: the non-`String` callers

The `Preconditions` overrides threw `ArrayIndexOutOfBoundsException` where the
JDK throws plain `IndexOutOfBoundsException` -- a *subclass*, i.e. wrong in the
direction that breaks a `catch`. `RuntimeError` had no plain variant; it has one
now (`RuntimeError::ioobe`), and all five overrides use it with HotSpot's own
message text. `checkFromIndexSize` also stopped overflowing on `from + size`,
which is why the JDK prints that addition unevaluated.

`StringPolicyMatrixProbe` cannot see any of this -- every out-of-bounds row in
it is `String`-domain, and those are already served by the F4 bypass.
`probes/NioOutOfBoundsClassProbe` exists for exactly that blind spot and checks
the direct callers against a HotSpot control:

```text
Objects.checkIndex(9,8)        IndexOutOfBoundsException  "Index 9 out of bounds for length 8"
Objects.checkFromToIndex(3,2)  IndexOutOfBoundsException  "Range [3, 2) out of bounds for length 8"
Objects.checkFromIndexSize     IndexOutOfBoundsException  "Range [6, 6 + 5) out of bounds for length 8"
```

All three now match HotSpot exactly, class and message.

## Where this stands

**Behaviourally closed.** Nothing observable is known to differ from HotSpot for
either the `String` or the NIO callers.

**Mechanically still open**, and worth keeping this record for:

* The `BiFunction` is never invoked. The overrides reproduce the two formatters'
  output rather than calling them, so a caller passing a *custom* formatter --
  legal, and what the four-argument overloads exist for -- still gets the
  built-in wording. No JDK or library code in this VM's corpus is known to do
  that, which is why this is a note rather than a defect.
* **F4's three `java/lang/String.check*` natives are therefore still load-bearing
  and must not be deleted.** They are what keeps the `String` domain on the
  right exception class; `Preconditions` reaching parity does not make them
  redundant, because they exist to bypass the formatter question entirely.
  `the_surviving_string_registration_set_is_exactly_this` pins all three.

## Found while fixing this, NOT caused by it

`probes/NioOutOfBoundsClassProbe` turned up two unrelated divergences on the
same run. Recorded here because the probe is the reproducer, not because they
belong to this record:

* **`ByteBuffer.get(99)` does not throw at all** -- an out-of-range absolute
  read returns silently. That is a missing bounds check, not a wrong class, and
  changing which exception `Preconditions` throws cannot produce a NO-THROW.
* **`List.of("a").get(3)` throws `ArrayIndexOutOfBoundsException`** where HotSpot
  throws `IndexOutOfBoundsException: Index: 3 Size: 1`. It kept the old class
  through this change, which is itself the evidence that it does not route
  through `Preconditions`.

Both need their own owner.

## What must change

Make the four-argument override honour its formatter: invoke the `BiFunction`
to build the exception object and throw that, falling back to
`IndexOutOfBoundsException` (the JDK's own fallback for a `null` formatter)
rather than `ArrayIndexOutOfBoundsException`. Then F4 is redundant and the two
`String` registrations can be deleted — which is the test that it worked.

## Verification when fixed

`probes/StringPolicyMatrixProbe` rows 83-88 (`substring1(PLAIN,-1)`,
`substring1(PLAIN,len+1)`, `substring2(PLAIN,3,2)`, `substring2(PLAIN,-1,3)`,
`substring2(PLAIN,0,len+1)`, `substring2(PLAIN,MIN,MAX)`) plus
`new String(utf8,-1,2,"UTF-8")` and `new String(utf8,0,999,"UTF-8")`. All eight
want the class **and** the message; the class is the half that changes
behaviour. Add a non-`String` case (an NIO buffer slice) to cover the second
defect above, which F4 does not touch.
