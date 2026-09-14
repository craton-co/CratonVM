# F2-1 — the `CharBuffer` that read empty, the shift that allocated 256 MB, and the constructor that invented a zero

**Status: LANDED in `native-builtins/src/phases_late.rs` (the only source file
this lane owns), plus three NOMINATIONS in files it does not. This lane cannot
build or run CratonVM: every statement about VM behaviour is labelled
SOURCE-READ or PREDICTED. Every statement about HotSpot is MEASURED on Microsoft
OpenJDK 25.0.3+9, with the transcript quoted. The new pure logic additionally
compiles and passes a 46-row differential against that transcript
(`scratchpad/f2/parity.rs`) — that is `rustc` on a standalone transliteration,
not a CratonVM build.**

Probes: `scratchpad/f2/{HexProbe.java, BiProbe.java, parity.rs}`.

Three vectors, from the parent lane:

1. `RJdkIntrinsics2 --only=hex` fails, MEASURED ON THE FINAL BINARY, at
   *"the char[] overload must honour its own offset/length"*.
2. `BigInteger.shiftRight(Integer.MIN_VALUE)` allocates ~256 MB.
3. `BigInteger.<init>([B)V` / `<init>(I[B)V` validate nothing.

(2) and (3) are NOMINATIONS 1 and 2 of
`E38-1-biginteger-shifts-ctors-and-the-stringbuilder-repeat-twin.md`, aimed at
this lane's file. Both anchors were verified before application; NOMINATION 1's
replacement text does not compile as written and is landed a different way, for
a reason worth recording (§2.1).

---

## 1. `HexFormat.parseHex` — the range overloads were never registered, and that was not a `NoSuchMethodError`

### 1.1 What the fixture was actually seeing

`register_p64_hex_format` registered `parseHex` twice — `(Ljava/lang/String;)[B`
and `(Ljava/lang/CharSequence;)[B` — and neither of them takes a range. The two
RANGED overloads were absent. In real-JDK mode an absent registration is not an
error: the JDK's own bytecode runs. So

```java
    HexFormat.of().parseHex(chars, 1, 5)
```

ran `HexFormat.java:577-582`, which is

```java
    public byte[] parseHex(char[] chars, int fromIndex, int toIndex) {
        Objects.requireNonNull(chars, "chars");
        Objects.checkFromToIndex(fromIndex, toIndex, chars.length);
        CharBuffer cb = CharBuffer.wrap(chars, fromIndex, toIndex - fromIndex);
        return parseHex(cb);
    }
```

— and that last line handed a **`CharBuffer`** to the one-argument native, whose
reader was

```rust
    Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default().chars().collect(),
```

`NativeContext::read_string` is class-guarded (`vm_exec.rs:11713`: *"reject any
object whose class is known and is not java/lang/String"*), so a `CharBuffer`
answers `None`, `unwrap_or_default()` turns that into `""`, and **`parseHex`
returned a zero-length array**. Not an exception, not a wrong byte — an empty
answer, for every range, on the only path the JDK itself uses.

That is why the failure reads as a *wrong value* rather than a throw, and it is
the fixture's own thesis about `HexFormat` — a configuration object with no
reader — repeated one level down: an **argument** with no reader.

The same hole was open for any application call: `parseHex(new
StringBuilder("00ff"))` and `parseHex(CharBuffer.wrap("00ff"))` are both legal
(the parameter is `CharSequence`) and both answered `[]` where HotSpot answers
`[0, -1]`. MEASURED, HotSpot:

```text
of().parseHex(new StringBuilder("00ff"))    =  [0, -1]
of().parseHex(CharBuffer.wrap(cs, 1, 4))    =  [0, -1]
```

`[reach≠defect]` in reverse: the fixture reached the family through `String`
only, so the family only ever worked for `String`.

### 1.2 The contract, measured

`HexProbe.java`, on HotSpot 25.0.3+9. Four exception classes, and the order of
the checks is load-bearing:

```text
of().parseHex(char[]{x00ff0a80}, 1, 5)   =  [0, -1]
of().parseHex(char[4], 4, 4)             =  []
of().parseHex(char[4], 3, 1)  !! IndexOutOfBoundsException: Range [3, 1) out of bounds for length 4
of().parseHex(char[4], -1, 2) !! IndexOutOfBoundsException: Range [-1, 2) out of bounds for length 4
of().parseHex(char[4], 0, 9)  !! IndexOutOfBoundsException: Range [0, 9) out of bounds for length 4
of().parseHex(char[4], 1, 4)  !! IllegalArgumentException: string length not even: 3
of().parseHex((char[]) null, -5, 99)      !! NullPointerException: chars
of().parseHex((CharSequence) null, -1, 9) !! NullPointerException: string
of().parseHex((CharSequence) null)        !! NullPointerException: Cannot invoke
                                             "java.lang.CharSequence.length()" because "string" is null
```

Five things a source read alone would have got wrong:

* **The `char[]` parameters are `(fromIndex, toIndex)`, not `(offset,
  length)`** — the class javadoc calls them "offset, length" at
  `HexFormat.java:79` and the signature at `:577` is `fromIndex, toIndex`. The
  fixture's message says "offset/length"; its *expectation*, `(1, 5)` over
  `"x00ff0a80"` giving two bytes, is `fromIndex/toIndex`. Believing the prose
  would have produced a one-character-longer slice on every call.
* The null check **precedes** the bounds check on the ranged forms: a null
  operand with an out-of-range pair reports the null.
* But the *message* differs between the one- and three-argument forms: the
  one-argument form has no `requireNonNull` and fails inside `string.length()`,
  so it carries a helpful-NPE, while the ranged forms name the parameter
  (`"chars"`, `"string"`).
* `Range [f, t) out of bounds for length n` counts the WHOLE operand;
  `string length not even: n` counts the **slice**.
* An empty range at the very end (`4, 4` on a length-4 operand) is legal.

### 1.3 UTF-16: the parser was on `Vec<char>` and the indices are code units

`parseHex`'s range is `string.length()`-shaped, i.e. UTF-16 code units, and the
digit error names the **unit**:

```text
of().parseHex(char[]{D801,DC37}, 0, 2) !! NumberFormatException: not a hexadecimal digit: "?" = 55297
of().parseHex("\uD801")                !! IllegalArgumentException: string length not even: 1
of().parseHex("١١")          !! NumberFormatException: not a hexadecimal digit: "?" = 1633
```

55297 is `\uD801` itself, not the pair's code point 66615, and the lone
surrogate counts as **one** character. A `Vec<char>` reader gets both wrong: it
fuses a surrogate pair into one `char` (so the astral row becomes code point
66615) and shortens the sequence by one (so a two-unit operand can become an
*odd* length and change the exception CLASS from `NumberFormatException` to
`IllegalArgumentException`). The parser, `p64_hex_digit_value` and both
`fromHexDigits` readers now run on `&[u16]`.

RESIDUAL, deliberate: a Rust `String` cannot hold a lone surrogate, so the
quoted character in the message renders as U+FFFD where HotSpot emits the raw
unit. The code point — the comparable part — is exact.

### 1.4 What landed

* **`parseHex([CII)[B` and `parseHex(Ljava/lang/CharSequence;II)[B` registered**,
  over three new shared helpers: `p64_hf_check_from_to` (the
  `Objects.checkFromToIndex` message), `p64_hf_char_array_units`, and
  `p64_hf_seq_units`.
* **`p64_hf_seq_units` is the reader the family was missing**, and it did not
  need writing: `charset_buffers::read_wrapped_char_sequence` has been in this
  tree all along, documented with the identical diagnosis (*"without it every
  non-`String` `CharSequence` reads back as empty, which is silent"*) for
  `CharBuffer.wrap`. `[1 of 10 callsites]`: the correct helper existed, one
  family used it, `HexFormat` did not. A real `String` still takes
  `lang_string::read_string_chars`, the lossless UTF-16 reader, so no allocation
  or interning path moves for ordinary text.
* **`fromHexDigits(CharSequence,II)I` and `fromHexDigitsToLong(CharSequence,II)J`
  registered** over the same helpers. These were *correct* in real-JDK mode
  (bytecode) and absent in synthetic-jdk mode — half a family registered, which
  is `[flag≠mode drops it]` waiting to happen.
* **`of()` is now a singleton.** MEASURED: `HexFormat.of() == HexFormat.of()` is
  `true` — `of()` is `return HEX_FORMAT;`. The cache is the class's **own static
  field**, resolved by `static_field_index_by_name`, not a Rust-side
  `OnceLock`: a static is a GC root (so the reference survives a moving
  collection) and it is per-VM, not per-process — `[vmscope]`, `[OnceLock=?]`.
  When the field cannot be resolved (a synthetic stub that does not declare it)
  the helper answers `None` and the old fabricate-every-time behaviour stands,
  so nothing regresses for that mode.
* **GC-SAFETY**: every ranged native now reads the receiver's configuration
  BEFORE it reads the sequence, because `p64_hf_seq_units` can re-enter Java
  (`CharBuffer.toString()`) and a collection there may move the receiver,
  leaving the `this` handle in `args[0]` stale. `P64HexCfg` is Rust-owned, so
  nothing on the Java heap is held across the re-entry.

### 1.5 Evidence

`parity.rs` transliterates `p64_hex_digit_value`, `p64_hf_check_from_to`,
`p64_parse_hex_chars`, the two ranged compositions and the shift guard into a
standalone Rust program and diffs them against the HotSpot transcript:

```text
PARITY rows=46 diffs=0
```

**This proves the constants, the message text and the control flow, and it
proves that much of the new code compiles. It does not prove the file compiles**
— the `NativeContext` plumbing (`read_string`, `read_wrapped_char_sequence`,
`static_field_index_by_name`, `get_array_element`) is stubbed out in the probe.

---

## 2. `BigInteger` — E38-1's NOMINATION 1, and why its text could not be applied verbatim

### 2.1 The anchor was right; the call was not reachable

NOMINATION 1's OLD text matched `phases_late.rs` byte for byte. Its NEW text is

```rust
            crate::math_bignum::bi_checked_shl(&v, n as u32)?
```

and `bi_checked_shl` is a **private `fn`** in `math_bignum.rs` — a file this
lane does not own, so the two-word visibility change the record offers to make
"on request" cannot be made here, and the nomination as written would not
compile. Landing non-compiling code is worse than not landing the fix, so the
guard is landed **in this file** as `p71_bi_checked_shl` + `p71_bi_mag_bits`,
with the same rule, the same constant and the same message, and the duplication
is declared in the doc comment and nominated away below (NOMINATION 1).

This is worth recording as a shape, not just an incident: **a nomination whose
NEW text calls across an ownership boundary is only applicable if the callee is
already public.** Checking that the anchor matches is not enough; the *call* has
to be reachable from the nominated file too.

### 2.2 The rule, measured

`BiProbe.java` (`-Xmx3g`):

```text
ONE.shiftRight(MIN)  !! ArithmeticException: BigInteger would overflow supported range   [103 ms]
(-1).shiftRight(MIN) !! ArithmeticException: BigInteger would overflow supported range   [ 84 ms]
ONE.shiftLeft(MAX)   !! ArithmeticException: BigInteger would overflow supported range   [ 28 ms]
ONE.shiftLeft(MIN)    = 0        ZERO.shiftRight(MIN) = 0        ONE.shiftRight(MAX) = 0
ONE.shiftLeft(MAX-1)  = <signum=1 bitLength=2147483647>                                  [ 32 ms]
(2).shiftLeft(MAX-2)  = <signum=1 bitLength=2147483647>                                  [ 29 ms]
(-2).shiftLeft(MAX-2) = <signum=-1 bitLength=2147483646>                                 [161 ms]
```

The refusals are HotSpot allocating the oversized `int[]` and only then failing
`checkRange` — that is what the 28-103 ms rows are. CratonVM's arm allocated the
same `vec![0u32; 67_108_864]` and then *succeeded*, writing 67 M elements into a
Java `int[]`. **An unbounded allocation reachable from one ordinary call with an
attacker-chosen `n` outranks a wrong answer**, and it is reachable from
`shiftLeft` too: `shiftLeft(Integer.MAX_VALUE)` is the same 256 MB.

The last row is why the guard is on the **magnitude** bit count and not on
`BigInt::bit_length()`: `(-2).shiftLeft(MAX-2)` is legal and reports
`bitLength = 2147483646`, one less than its 2147483647 magnitude bits, because
`bitLength()` subtracts the sign bit for a negative exact power of two. A guard
written on `bit_length()` admits one more bit than HotSpot does for exactly that
family of operands — which is where the 256 MB would come back.

`unsigned_abs` is untouched: it is the JDK's own rule
(`BigInteger.java:3494-3506`), exactly as E38-1 established.

### 2.3 Both modes are now guarded, before the nomination lands

`register_biginteger_natives` (synthetic-jdk only, registered later, therefore
the winner there) already carries E38-1's guarded shifts;
`register_p71_biginteger_extras` (every mode, the winner in real-JDK and
compatible mode) now carries this one. The 256 MB is closed in **both** modes
today, at the cost of two implementations of one rule — which NOMINATION 1
collapses to one.

---

## 3. The two byte-array constructors — E38-1's NOMINATION 2

MEASURED contract (`BiProbe.java`), all fifteen rows:

```text
new BigInteger(new byte[0])          !! NumberFormatException: Zero length BigInteger
new BigInteger(new byte[]{0})         = 0
new BigInteger((byte[]) null)        !! NullPointerException: Cannot read the array length because "val" is null

new BigInteger(0, new byte[0])        = 0        new BigInteger( 1, new byte[0])     = 0
new BigInteger(0, new byte[]{0,0})    = 0        new BigInteger(-1, new byte[0])     = 0
new BigInteger(0, new byte[]{1})     !! NumberFormatException: signum-magnitude mismatch
new BigInteger(2, new byte[]{1})     !! NumberFormatException: Invalid signum value
new BigInteger(2, new byte[0])       !! NumberFormatException: Invalid signum value
new BigInteger(MIN_VALUE, byte[]{1}) !! NumberFormatException: Invalid signum value
new BigInteger(2, (byte[]) null)     !! NullPointerException: Cannot read the array length because "magnitude" is null
new BigInteger(-1, new byte[]{1})     = -1       new BigInteger(1, new byte[]{-1})   = 255
```

Four ordering facts, none of them guessable:

* **The null check precedes the signum check.** `new BigInteger(2, null)` is the
  NPE, not `"Invalid signum value"` — because the 2-argument constructor is
  `this(signum, magnitude, 0, magnitude.length)` and `magnitude.length` is
  evaluated at the **delegating call site**, before the 4-argument
  constructor's first statement.
* **The signum check precedes the empty-magnitude shortcut**:
  `new BigInteger(2, new byte[0])` still throws.
* A magnitude that **strips to nothing** is the value 0 whatever the signum
  says, so `signum == 0` must be tested against the *stripped* magnitude:
  `new BigInteger(0, new byte[]{0})` is legal and `new BigInteger(0, new
  byte[]{1})` is the mismatch.
* `new BigInteger(byte[])`'s length check is on the **whole array**, before
  anything else.

Before this commit `<init>([B)V` answered **0** for a zero-length array
(`bi_from_byte_array_signed(&[])` is `"0"`) and `<init>(I[B)V` accepted any
`i32` signum. A zero-length array is what a truncated read or an empty frame
hands a decoder, so the wrong answer is silent, and the value it invents — zero
— is the one that compares equal to nothing and verifies no signature.

All four checks landed, in the JDK's order, plus `p71_bi_array_arg` for the two
helpful NPEs (the generic `obj_arg` NPE carries no message at all). Neither
constructor is shadowed by `math_bignum.rs`, which registers only the two
`String` forms — so unlike the shifts, these fixes govern **both** modes on
their own.

---

## 4. What the fixtures should do — PREDICTED, every row

### `RJdkIntrinsics2 --only=hex` (73 checks)

The family stops at its first failure, so the reported message names check
**32 of 73**: call site 29, `parseHex(char[], 1, 5)` (the four-row `badRange`
loop at call site 23 accounts for the offset). 41 checks have never run.

| check | row | before | after | who answers |
|---|---|---|---|---|
| 32 | `parseHex(char[]{x00ff0a80}, 1, 5)` | **RED** (`[]`) | **green** | new `([CII)[B` |
| 33 | `ofDelimiter(":").parseHex("00:ff:0a:80")` | unreached | green | 1-arg native |
| 34 | prefix+suffix+delimiter round trip | unreached | green | 1-arg native |
| 35-38 | `"0f0"` IAE, `"zz"` NFE, `(seq,0,9)` IOOBE, `null` NPE | unreached | green | check 37 is the new `(CharSequence;II)[B` |
| 39-40 | delimited formatter rejects undelimited; default rejects prefixed | unreached | green | 1-arg native |
| 41-44 | `fromHexDigits("ff"/"FF"/"ffffffff"/"")` | unreached | green | 1-arg native |
| 45 | `fromHexDigits("abcdef", 2, 4) == 205` | unreached | green | new ranged static |
| 46-47 | `fromHexDigitsToLong` ×2 | unreached | green | 1-arg native |
| 48-49 | nine digits IAE, `"0x1f"` NFE | unreached | green | 1-arg native |
| 50-53 | `fromHexDigit` ×4 | unreached | **UNKNOWN** | unregistered — real JDK bytecode (`DIGITS`, needs `<clinit>`); a `NoSuchMethodError` in synthetic-jdk mode |
| 54-57 | `isHexDigit` ×4 | unreached | green | native |
| 58-62 | `toLowHexDigit` / `toHighHexDigit` ×5 | unreached | **likely green** | unregistered; pure, reads only `ucase` at slot 3 |
| 63-67 | `toHexDigits(char/short/long,int)` ×5 | unreached | **UNKNOWN** | unregistered; the JDK bodies go through `SharedSecrets`' `JavaLangAccess.uncheckedNewStringNoRepl` |
| 68 | `formatHex(Appendable, hb)` | unreached | likely green | unregistered; `toHighHexDigit` + `append` |
| 69 | `toString()` | unreached | green | native |
| 70-72 | `equals` / `hashCode` / not-equals | unreached | likely green | unregistered; real bodies over the four fields |
| 73 | `f == HexFormat.of()` — SINGLETON | unreached | **green** | `of()` now returns the cached `HEX_FORMAT` |

**PREDICTED: `--only=hex` advances from a stop at check 32 to, at worst, check
50 — and to a clean `CK RJdkIntrinsics2 hex=73` if the twelve unregistered
real-JDK rows behave.** The three UNKNOWN blocks are all "unregistered triple,
served by JDK bytecode", which is the *next* thing to measure, and all of them
are certainly red in synthetic-jdk mode.

### `RJdkBridge1 --only=bigint` (55 checks)

E38-1 predicted `bigint` would go from ~14 failing to 3 failing, "and the three
left are all NOMINATION 2". Those three:

| row | before | after |
|---|---|---|
| `new BigInteger(byte[0])` → NFE | RED | **green** |
| `new BigInteger(2, byte[]{1})` → NFE | RED | **green** |
| `new BigInteger(0, byte[]{1})` → NFE | RED | **green** |

**PREDICTED: `bigint` goes to 0 failing** (given E38-1's own edits, which this
lane did not verify). The shift rows the fixture exercises
(`ONE.shiftLeft(MIN)`, `16.shiftLeft(-2)`, `16.shiftRight(-2)`) are unchanged by
the new guard — none of them is a left shift past `Integer.MAX_VALUE` bits. The
256 MB row, `ONE.shiftRight(Integer.MIN_VALUE)`, **is not in the fixture at
all**; it is reachable only from an application, which is what made it worth
fixing ahead of a red row.

---

## NOMINATIONS

### 1. `native-builtins/src/math_bignum.rs` — delete the two shift registrations and the helper trio

E38-1 §1 says this itself: *"Once this lands, `shiftLeft`/`shiftRight` should be
deleted from `register_biginteger_natives` as well, finishing the
consolidation."* NOMINATION 1 has now landed (§2), so this is that follow-up.
With the p71 registrar guarded, the synthetic-only copies serve no purpose and
the file has zero overlap with `p71` once they go. `bi_checked_shl`,
`bi_mag_bits` and `bi_shift_arg` become dead code with them and must be deleted
in the same edit or the build warns.

OLD (`math_bignum.rs:1470`, through the end of the `shiftRight` registration at
`:1490`) — the two `registry.register(bi, "shiftLeft"/"shiftRight", ...)` blocks
and the three helper `fn`s at `:1724-1787`, verbatim as they stand.

NEW: deleted. Nothing replaces them — `register_p71_biginteger_extras`
(`phases_late.rs`, reached from `register_essential_natives`) registers both
triples in **every** mode, with the guard.

If that consolidation is not wanted, the one-word alternative is
`pub(crate) fn bi_checked_shl` + `pub(crate) fn bi_mag_bits`, after which
`phases_late`'s `p71_bi_checked_shl` / `p71_bi_mag_bits` should be deleted
instead and the two call sites pointed at `crate::math_bignum::`. Either way,
**one** of the two copies has to go.

### 2. `native-builtins/src/bigint.rs` — make `BigInt::mag_bits` `pub(crate)`

```rust
    /// Number of bits in the minimal magnitude (highest set bit + 1; 0 for zero).
    fn mag_bits(mag: &[u32]) -> usize {
```

is the same rule a **third** time (`bigint.rs:683`, `math_bignum.rs:1735`,
`phases_late.rs`'s new `p71_bi_mag_bits`). It is private, so neither caller can
reach it. Making it `pub(crate)` — or adding
`pub(crate) fn magnitude_bits(&self) -> u64` beside `bit_length()` — lets both
outside copies be deleted, which is what NOMINATION 1 needs to be a clean
deletion rather than a move.

### 3. `regression-suite/src/RJdkIntrinsics2.java` — the message says "offset/length" and the row asserts `fromIndex/toIndex`

```java
        check(Arrays.equals(f.parseHex("x00ff0a80".toCharArray(), 1, 5),
                        new byte[] { 0, (byte) 0xff }),
                "the char[] overload must honour its own offset/length");
```

The expectation is correct and MEASURED; the message is not. `parseHex(char[],
int, int)` takes `(fromIndex, toIndex)` — the "offset, length" wording comes
from the class javadoc at `HexFormat.java:79`, which contradicts the signature
at `:577`. An implementer who fixes this row by reading its own failure message
will write `chars[offset .. offset+length]` and be wrong by one character on
every call.

Suggested message: `"the char[] overload's (fromIndex, toIndex) must be a
half-open RANGE — the class javadoc calls them offset/length and the signature
does not"`.

While that file is open: the family has no row for a non-`String`
`CharSequence` through the one-argument `parseHex`, which is the defect §1.1
found, and no row for `parseHex(char[], 3, 1)` — the reversed range that used to
be the panic-shaped one everywhere else in this file.

---

## Honest summary

Three fixes, one of which was not the fix that was asked for. The
`--only=hex` failure was reported as a missing offset/length *computation*; it
is a missing **registration**, and the wrong answer came from a `CharBuffer`
being read by a `String`-only reader four call levels away from anything named
`parseHex`. The correct reader already existed, in this tree, with the identical
diagnosis written in its own doc comment — one more instance of
`[1 of 10 callsites]`, on a session that had already found twelve.

The second finding is procedural: E38-1's NOMINATION 1 was correct in its
diagnosis, its anchor and its constant, and still could not be applied as
written, because its replacement text calls a private function across an
ownership boundary. A nomination needs the callee's visibility checked, not only
the anchor's text.

And the third is that neither `BigInteger` defect the parent lane named is
visible in any fixture. `bigint`'s three red rows are the constructors; the
256 MB shift is reachable only from an application. It was fixed first anyway,
because an unbounded allocation from ordinary bytecode is not a wrong answer —
it is a denial of service that no Java `catch` can see.
