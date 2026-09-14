# G26-1 — four families of `RJdkIntrinsics3`, and the one that turned out to be mine

**Status:** MIXED, and the mix is the finding. The **before is MEASURED on both
VMs for all four assigned families**, family-wide rather than one row deep. The
fixes that landed are in `native-builtins/src/lang_string.rs` and their **after
is PREDICTED**, because this lane was forbidden to build (§9). **Not one of the
four assigned families is served by a body in this lane's three files** — that
was checked with `--dump-native-registry`, not by reading, and it is §1.

**Provenance:** MEAS on both VMs. Oracle: HotSpot 25.0.3+9-LTS
(`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`). VM under test:
`C:/craton/target-fcheck/release/cratonvm.exe`, `--java-home` at that JDK,
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`. Probes:
`scratchpad/g26/src/G26{Surrogate,Builder,Builder2,Insert,InsCs,Buf,Inet,Fmt,Misc,Surrog,Vo}.java`,
compiled `-XDstringConcat=inline` so nothing here can fail on
`StringConcatFactory`. 402 probe rows across nine files, every one run on both
VMs and diffed with line endings normalised.

This lane owns exactly three files: `lang_string.rs`, `inet_address.rs`,
`phases_late/nio_buffer.rs`. Everything else is a NOMINATION (§8).

---

## 0. The headline

| what | before (MEASURED) | after |
|---|---|---|
| `RJdkIntrinsics3` `--only=fmtobj` | RED row 1 of 22; **16 of 38 probe rows wrong** | unchanged — NOMINATION N1, `lib.rs` |
| `RJdkIntrinsics3` `--only=inet` | RED; **3 of 51 probe rows wrong**, all one rule | unchanged — NOMINATION N2, `phases_early.rs` |
| `RJdkIntrinsics3` `--only=bufslice` | RED; **5 of 99 probe rows wrong**, all one body | unchanged — NOMINATION N3, `lib.rs` |
| `RJdkIntrinsics3` `--only=misc` | RED; 2 blockers, `new String(sb)` then `new EnumMap(null)` | unchanged — NOMINATIONS N4, N6 |
| `RJdkIntrinsics3` `--only=regex` | **GREEN, 42** | unchanged |
| `RJdkIntrinsics3` `--only=mathexact` | **GREEN, 57** | unchanged |
| the `StringBuilder`/`StringBuffer` text surface | **28 of 66 rows wrong**, plus one **VM ABORT** | PREDICTED 0 of 66, no abort |
| `insert(int, Object)` for a non-`String` | **5 of 5 rows wrong** — inserts the text `"null"` | PREDICTED 0 of 5 |
| `insert(int, CharSequence[, int, int])` | **8 of 8 rows wrong** — silently OVERWRITES | PREDICTED 0 of 8 |
| `RJdkBridge1` `--only=surrog` | RED at check 1 of 22; **8 of 28 replayed rows wrong** | PREDICTED **GREEN, 22** (§6) |
| `RJdkStringCodePoints` / `RStrings` / `RJdkNet` | GREEN 186 / 46 / 81 | re-run gate |
| `RDirectBufferElem` / `RJdkByteOrder` / `RJdkFormatLocale` | GREEN 506 / 84 / 20 | re-run gate |

The oracle passes `RJdkIntrinsics3` with 1011 checks. On the current binary the
vector reaches **741**, unchanged from G21-1's measurement.

---

## 1. The brief was wrong about where the four families live, and the dump says so

The assignment said all four failing families "fall in your files". They do not.
`--dump-native-registry`, taken against each family's own `--only=` workload so
`invocations` is meaningful (G21-1 §2's correction), places every one of them
elsewhere:

| family | failing triple | `registered_by` | `owns_slot` | `invocations` |
|---|---|---|---|---|
| `fmtobj` | `java/util/Formatter.out()Ljava/lang/Appendable;` | **`lib.rs:21725`** | true | 1 |
| `fmtobj` | `java/util/Formatter.<init>()V` | **`lib.rs:21574`** | true | 1 |
| `inet` | `java/net/InetSocketAddress.createUnresolved` | **`phases_early.rs:12953`** | true | 6 |
| `bufslice` | `java/nio/ByteBufferAsCharBufferB.get()C` | **`lib.rs:34972`** | true | 4 |
| `misc` | `java/lang/String.<init>(Ljava/lang/StringBuilder;)V` | **`deprecated_util.rs:2043`** | true | 3 |

Every one is `owns_slot=true` with `overwrote=null`, so there is no second
registrant to blame and no ambiguity about which body runs. In `--jdk-only`
this lane's `lang_string.rs` owns **185 slots and they are all
`StringBuilder`/`StringBuffer`/`AbstractStringBuilder`/`StringUTF16`** — every
`java/lang/String` and `java/util/Formatter` registration in that file is a
`Bridge` and is dropped in real-JDK mode. `inet_address.rs` owns only the
`Inet4/6AddressImpl` resolver surface, which nothing in `inet` touches (the
family performs no DNS lookup by construction). `nio_buffer.rs` is **DEFAULT
OFF** (`CRATONVM_BYTEBUFFER_INTRINSIC`) and registers only on
`java/nio/HeapByteBuffer`, which is not the class `bufslice` fails on.

That is not a complaint, it is the reason §2 exists: the family-wide sweeps the
brief asked for were done anyway, in full, so the four owning lanes get a
transcribed contract instead of a first failing assertion. And sweeping the one
family that *is* mine found a larger defect than any of the four.

---

## 2. The four assigned families, measured whole

### 2.1 `fmtobj` — 16 of 38 rows, and `out()` is the smallest of them

`scratchpad/g26/src/G26Fmt.java`, 38 rows.

| row | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `new Formatter().out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `out() instanceof StringBuilder` | `true` | **`false`** |
| `out() instanceof Appendable` | `true` | **`false`** |
| `new Formatter(Locale.ROOT).out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `new Formatter((Locale) null).out()` class | `java.lang.StringBuilder` | **`java.lang.String`** |
| `new Formatter((Appendable) null).out()` class | `java.lang.StringBuilder` | **`<null>`** |
| `toString()` after `close()` | `FormatterClosedException` | **returns** |
| `out()` after `close()` | `FormatterClosedException` | **returns** |
| `flush()` after `close()` | `FormatterClosedException` | **returns** |
| `format()` after `close()` | `FormatterClosedException` | **returns** |
| `locale()` after `close()` | `FormatterClosedException` | **returns** |
| the caller's buffer after a post-close `format` | `pre:005` | **`pre:0051`** |

`close()` is a no-op: the last row is the proof, because a `format()` that was
supposed to throw appended a `1` to the caller's `StringBuilder` instead. The
vector's `fmtobj` block calls that out explicitly — *"the load-bearing rows are
the post-close ones"* — and the family would still be red after an `out()`-only
fix, at check 15 of 22 rather than check 1.

Twenty-two of the 38 rows already match, including the whole locale axis
(`locale()` of the no-arg, of `ROOT`, of an explicit `null`, and the grouping
output of each) and `Formatter(Appendable)`'s identity row. G21-1's report that
a sibling lane found the locale slot and the conversion table already correct is
confirmed. **NOMINATION N1.**

### 2.2 `inet` — 3 of 51 rows, and the ordering is the part that cannot be derived

`scratchpad/g26/src/G26Inet.java`, 51 rows, no DNS.

| row | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `createUnresolved(null, 80)` | `IllegalArgumentException: hostname can't be null` | **`NullPointerException: null object argument`** |
| `createUnresolved(null, -1)` | `IllegalArgumentException: port out of range:-1` | **`NullPointerException: null object argument`** |
| `new InetSocketAddress((String) null, 80)` | `IllegalArgumentException: hostname can't be null` | **`NullPointerException: null object argument`** |

Three things here are transcription, not derivation:

* the message is `hostname can't be null` — an apostrophe, no article;
* the port message is `port out of range:-1`, with **no space after the colon**;
* **the port check runs FIRST.** `createUnresolved(null, -1)` is *both* wrong,
  and HotSpot answers the port. A guard written as "null host first" gets row 1
  right and row 2 wrong, and row 2 is the one a reviewer would call obviously
  equivalent.

Everything else on the construction surface already matches, and the parts most
likely to be wrong are among them: `new InetSocketAddress((InetAddress) null,
80)` is **legal** and yields the wildcard (`0.0.0.0/0.0.0.0:80`), 65535 is
accepted and 65536 refused on all four constructors, `getHostString` and
`getHostName` agree on an unresolved address and on a numeric literal but differ
on the loopback (`localhost` vs the same), a resolved address never equals an
unresolved one with the same text in either direction, and host comparison is
case-insensitive (`Example.Invalid` equals `example.invalid`). **NOMINATION N2.**

### 2.3 `bufslice` — 5 of 99 rows, and all five are one body

`scratchpad/g26/src/G26Buf.java`, 99 rows, sweeping `slice()`, `slice(int,int)`,
`duplicate`, `asReadOnlyBuffer`, absolute vs relative `get`/`put` past the
limit, on heap, direct and read-only buffers, and on each typed view.

| row | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `ByteBufferAsCharBufferB.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| `ByteBufferAsCharBufferL.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| `ByteBufferAsCharBufferRB.get()` at the limit | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| the same view's `slice()` | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |
| the same view's `duplicate()` | `BufferUnderflowException` | **`IndexOutOfBoundsException`** |

The three class names are one family: `slice()` and `duplicate()` of a
`ByteBufferAsCharBufferB` are `ByteBufferAsCharBufferB`, and the read-only form
is `ByteBufferAsCharBufferRB`, which inherits the same body. So this is **one**
registration, not five.

The discriminating rows are the ones on the SAME class that already match:

| call on `ByteBufferAsCharBufferB` | HotSpot | CratonVM |
|---|---|---|
| `get(int)` out of range | `IndexOutOfBoundsException` | = |
| `charAt(int)` out of range | `IndexOutOfBoundsException` | = |
| `put(int, char)` out of range | `IndexOutOfBoundsException` | = |
| `put(char)` past the limit | `BufferOverflowException` | = |
| `get()` past the limit | **`BufferUnderflowException`** | **wrong** |
| position after a failed `get()` | `4` (unchanged) | = |

The choice really is per-method on one class, exactly as the brief warned, and
CratonVM has three of the four right. `DirectCharBufferS`/`DirectCharBufferU`,
`HeapCharBuffer`, `HeapCharBufferR` and `StringCharBuffer` are separate classes
and all correct. So are 47 further rows covering `slice(2,3)` contents and
capacity, `slice(7,3)`/`slice(-1,2)`/`slice(0,9)` refusals, positioned
`slice()`/`duplicate()`, the read-only `put` family, `hasArray`/`array`/
`arrayOffset` on direct, read-only and view buffers, `get(byte[])` short reads,
and the rule that a failed relative get does not advance `position`.

`native-builtins/src/phases_late/nio_buffer.rs` — this lane's file — already
implements exactly this rule for `HeapByteBuffer`, in `relative::<WIDTH>` with
`buffer_underflow(ctx)` beside `index_out_of_bounds()`, and documents why the
two are not interchangeable. It is a working exemplar for the fix, and it is
DEFAULT OFF, so it is not the body `bufslice` reaches. **NOMINATION N3.**

### 2.4 `misc` — two blockers, neither in this lane's files

The vector dies on the lone surrogate. `scratchpad/g26/src/G26Surrogate.java`
(56 rows) locates it precisely:

| row | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `sb.append((char) 0xDC00); sb.charAt(0)` | `56320` | = |
| `sb.toString().charAt(0)` | `56320` | = |
| **`new String(sb).charAt(0)`** | `56320` | **`65533`** |
| `new String(char[]{'x',0xDC00,'y'})` | `U+0078 U+DC00 U+0079` | = |
| `new StringBuilder(loneString).charAt(0)` | `56320` | **`65533`** |
| `String.join("-", lone, lone)` | `U+DC00 U+002D U+DC00` | **`U+FFFD U+002D U+FFFD`** |

So the builder keeps the surrogate and `StringBuilder.toString()` keeps it —
`sb_string_from_units` in `lang_string.rs` already does the units write. The
loss is in `native_string_init_from_string_builder`
(`deprecated_util.rs:656`), whose last line is
`String::from_utf16_lossy(&chars).into_bytes()`. **NOMINATION N4**, and the fix
is one call: `crate::lang_string::sb_string_from_units`, which is `pub(crate)`,
already handles the pin and the well-formed fast path, and is documented as the
one place this construction should live.

Past that blocker, `scratchpad/g26/src/G26Misc.java` replays the rest of the
family: **19 rows, 1 wrong.**

| row | HotSpot | CratonVM |
|---|---|---|
| `new EnumMap((Class) null)` | `NullPointerException` | **returns** |

`PrintStream.charset()` on two different charsets, `EnumMap` ordinal iteration,
the `Iterator.remove()` illegal-state protocol both ways, and
`DateFormat.format` on `Asia/Kolkata` from both call sites are all already
correct. `java/util/EnumMap.<init>(Ljava/lang/Class;)V` is
`phases_early.rs:6084`, `owns_slot=true`, `invocations=2`. **NOMINATION N6.**

**W7-95a's standing claim is confirmed and narrowed.** A Rust `str` cannot hold
a lone surrogate, and that is the cause — but the write-side repair
(`init_string_from_units` on `NativeContext`, `sb_string_from_units` in
`lang_string.rs`) and the read-side repair (`read_string_chars`, layout-aware,
in the same file) are both **already in the tree and both work**. G9-1's
NOMINATION N4 proposed adding `read_string_units` to `NativeContext` as a
prerequisite; it is not one. `lang_string::read_string_chars` is that function,
it decodes the JDK 9+ compact `byte[]` layout correctly (measured through
`String.toCharArray()` and `getChars`, which round-trip a lone surrogate
byte-for-byte), and every fix in §3 uses it. **No new `NativeContext` method was
needed.**

---

## 3. What this lane actually fixed: the builder's text surface

Sweeping `lang_string.rs`'s real live surface — the 61 builder slots it owns in
`--jdk-only` — found a defect family larger than any of the four assigned ones.

### 3.1 The lone surrogate, 28 rows of 66

`scratchpad/g26/src/G26Builder.java` / `G26Builder2.java`. Every divergent row
is the same shape: the argument was read through `ctx.read_string` into a Rust
`String` and re-encoded, and the `str` in the middle cannot carry an unpaired
surrogate.

| entry point | rows | before |
|---|---|---|
| `new StringBuilder(String)` / `new StringBuffer(String)` | c1 c2 c3 c5 c7 | U+FFFD |
| `new StringBuilder(CharSequence)` (String, builder, StringBuffer) | c6 c8 c9 | U+FFFD |
| `append(String)`, on both classes | a1 a2 a12 | U+FFFD |
| `append(CharSequence)`, `append(CharSequence,int,int)`, `append(StringBuffer)` | a3 a5 a6 a10 a11 | U+FFFD |
| `append(Object)` | a4 | U+FFFD |
| `insert(int, String)` | i1 | U+FFFD |
| `insert(int, Object)` | i2 | U+FFFD |
| `replace(int, int, String)` | r1 | U+FFFD |
| `repeat(String,int)` / `repeat(CharSequence,int)` | r2 r3 | U+FFFD |
| downstream of the ctor: `reverse`, `delete`, `deleteCharAt`, `setLength`, `contentEquals` | o10 d1 d2 d3 e1 e4 | U+FFFD |

The `contentEquals` row is the one that shows how bad this is:
`s.contentEquals(new StringBuilder(s))` answered **false**. A builder and the
String it was constructed from disagreed about their own contents, silently,
with nothing thrown.

Twenty-one rows were already correct and are the controls: everything reached by
`append(char)`, `append(char[])`, `append(char[],int,int)`, `insert(int,char)`,
`insert(int,char[])`, `repeat(char,int)`, `appendCodePoint`, `setCharAt`, and
the whole read-out side — `toString`, `substring`, `charAt`, `codePointAt`,
`codePointBefore`, `codePointCount`, `getChars`, `chars()`, `compareTo`,
`indexOf(String)`, `lastIndexOf(String)`. **The read side of this file was
already lossless.** Only the entry points were not, so the file disagreed with
itself: `charsequence_fast_units`' fast path answered U+FFFD where its own
`charAt` walk — the slow path for every non-JDK `CharSequence` — answered
U+DC00, for the same sequence.

**The fix is at the two choke points, not at the eleven call sites.**
`charsequence_fast_text` becomes `charsequence_fast_units` and returns
`Vec<u16>`, which repairs `append(CharSequence)`, `append(CharSequence,int,int)`,
`repeat(CharSequence,int)`, `new StringBuilder(CharSequence)` and
`charsequence_length` at once; `invoke_to_string_opt` is refactored onto a new
`invoke_to_string_units_opt`, which repairs `append(Object)` and
`insert(int,Object)`. The `String`-typed entry points read `read_string_chars`
directly. `invoke_to_string`'s public `String` signature is unchanged — it is
now `String::from_utf16_lossy` of the units, which is byte-identical to what
`ctx.read_string` answered for every existing caller, so `phases_early.rs`,
`nio_file.rs`, `util_concurrent_ext.rs` and `xml_xerces.rs` see nothing move.

### 3.2 `insert(int, Object)` inserted the literal `"null"` for every non-String

`scratchpad/g26/src/G26Insert.java`. This is not a surrogate bug and had nothing
to do with the assignment; it was found by sweeping the family.

| call, on `new StringBuilder("ab")` | HotSpot | before |
|---|---|---|
| `insert(0, Integer.valueOf(7))` | `7ab` | **`nullab`** |
| `insert(0, obj whose toString() is "CUSTOM")` | `CUSTOMab` | **`nullab`** |
| `insert(0, Boolean.TRUE)` | `trueab` | **`nullab`** |
| `insert(0, Double.valueOf(1.5))` | `1.5ab` | **`nullab`** |
| `insert(0, new int[]{1})` | `[I@…ab` | **`nullab`** |

The body read `ctx.read_string(obj)`, which answers `None` for anything that is
not a `java.lang.String`, and the `None` arm was `"null"`. The sibling
`append(Object)` two hundred lines up has always used `invoke_to_string` and has
always been right — measured, rows n7 and n8. One overload of one method, one
helper apart from its twin, wrong for six years of every non-String argument.

### 3.3 `insert(int, CharSequence)` had no native, and the next `charAt` ABORTED THE VM

The registration list closed the four scalar `insert` overloads (W7-3) and left
the two reference ones open. Real `AbstractStringBuilder` bytecode therefore ran
against this VM's synthetic `char[]`/`count` layout:

| call, on `new StringBuilder("xy")` | HotSpot | before |
|---|---|---|
| `insert(1, (CharSequence) "AB")` | `xABy` | **`xA`** |
| `insert(1, (CharSequence) new StringBuilder("AB"))` | `xABy` | **`xA`** |
| `insert(1, seq)` (a custom `CharSequence`) | `xPQy` | **`xP`** |
| `insert(1, seq, 0, 1)` | `xPy` | **`xP`** |
| `insert(1, (CharSequence) null)` | `xnully` | **`xn`** |
| `insert(1, (CharSequence) null, 0, 4)` | `xnully` | **`xn`** |
| `new StringBuffer("xy").insert(1, (CharSequence) "AB")` | `xABy` | **`xA`** |
| `new StringBuffer("xy").insert(1, (CharSequence) "AB", 0, 1)` | `xAy` | **`xA`** |

It did not throw. It **overwrote and truncated**, which reads as a successful
insert of the wrong text — and it left the receiver's count and buffer
inconsistent, so the next `charAt` on it hit `buf.unwrap()` in
`native_sb_char_at` and terminated the process:

```
thread 'main-vm' panicked at native-builtins\src\lang_string.rs:2591:19:
called `Option::unwrap()` on a `None` value
```

A Rust panic is not a Java throwable — it does not unwind to the `catch` three
lines below, it kills the VM. That `unwrap` is now a refusal with the same class
and message the neighbouring bounds check uses, and the registration gap is
closed, so nothing reaches the arm any more. The guard stays because it must not
depend on that.

### 3.4 The `insert(int, CharSequence, …)` contract, TRANSCRIBED

`scratchpad/g26/src/G26InsCs.java`, run on HotSpot 25.0.3+9-LTS. The javadoc
names only the classes; every message and every ordering below is measured.

| call, on `new StringBuilder("xy")` | HotSpot |
|---|---|
| `insert(5, (CharSequence) "A")` | `StringIndexOutOfBoundsException: Range [5, 2) out of bounds for length 2` |
| `insert(-1, (CharSequence) "A")` | `StringIndexOutOfBoundsException: Range [-1, 2) out of bounds for length 2` |
| `insert(5, seq)` — a non-String | the same `StringIndexOutOfBoundsException` |
| `insert(5, (CharSequence) null)` | the same `StringIndexOutOfBoundsException` |
| `insert(1, "AB", 0, 9)` | `IndexOutOfBoundsException: Range [0, 9) out of bounds for length 2` |
| `insert(1, "AB", -1, 1)` | `IndexOutOfBoundsException: Range [-1, 1) out of bounds for length 2` |
| `insert(1, "AB", 2, 1)` | `IndexOutOfBoundsException: Range [2, 1) out of bounds for length 2` |
| `insert(9, "AB", 0, 9)` — BOTH wrong | the **offset** one: `Range [9, 2) …` |
| `insert(1, null, 0, 9)` | `IndexOutOfBoundsException: Range [0, 9) out of bounds for length 4` |
| `insert(1, null, 0, 4)` | no throw, `xnully` |
| `insert(1, "AB", 1, 1)` | no throw, `xy` — an empty window is legal |
| `insert(5, cs)` whose `length()` throws | **that `IllegalStateException`** |
| `insert(5, cs, 0, 1)` whose `length()` throws | the offset `StringIndexOutOfBoundsException` |

Two of these are not guessable:

* **The two refusals are different classes.** The offset is a
  `StringIndexOutOfBoundsException`; the window is the PLAIN
  `IndexOutOfBoundsException`. A caller catching the subclass must not see the
  window refusal, and a body funnelling both into one class passes an
  `instanceof Exception` test and fails a real one.
* **The last two rows disagree with each other.** Same receiver, same bad
  offset, same sequence, different exception — because the 2-arg form is
  `insert(dstOffset, s, 0, s.length())` and `s.length()` is an *argument*, so it
  is evaluated before the 4-arg call's offset check, while the 4-arg form
  checks the offset first. Either ordering looks equally reasonable from the
  javadoc. Both are now pinned by unit tests.
* And the null substitution happens **before** the range check, which is why
  `insert(1, null, 0, 9)` reports `length 4` and not the receiver's 2 — the same
  rule `native_sb_append_charsequence_off_len` already records, now sharing the
  same two message helpers so the overloads cannot drift.

### 3.5 One behavioural change beyond the surrogate

`new StringBuilder(CharSequence)` and `repeat(CharSequence, int)` used
`invoke_to_string(…).unwrap_or_default()`, which **swallowed** an exception from
the sequence and continued with the empty string. Routing them through
`charsequence_chars` propagates it, which is what HotSpot does (measured: a
`length()` that throws surfaces that exception). Recorded here rather than
buried, because it is the one place this change makes something throw that did
not throw before.

---

## 4. What changed, by name

All in `native-builtins/src/lang_string.rs`.

| site | change |
|---|---|
| `charsequence_fast_text` → `charsequence_fast_units` | returns `Vec<u16>`; `String` arm uses `read_string_chars`, builder arm uses `sb_read_chars` (identical to what `toString()` answers, minus a Java re-entry) |
| `charsequence_chars`, `charsequence_length` | consume units |
| `invoke_to_string_opt` | now a lossy view of the new `invoke_to_string_units_opt`; signature and every existing caller unchanged |
| `invoke_to_string_units_opt`, `invoke_to_string_units` | new; the dispatch, in units |
| `native_sb_init_string` | `read_string_chars` |
| `native_sb_init_charsequence` | `charsequence_chars` |
| `native_sb_append_string` | `read_string_chars` + `sb_append_chars` |
| `native_sb_insert_string` | `read_string_chars` |
| `native_sb_insert_object` | `invoke_to_string_units`, pinned across the re-entry |
| `native_sb_replace` | `read_string_chars` |
| `native_sb_repeat_charsequence` | `charsequence_chars` |
| `native_sb_char_at` | `buf.unwrap()` → a refusal |
| `native_sb_insert_charsequence`, `native_sb_insert_charsequence_range` | **new bodies** |
| `register_string_builder_natives` | **+2 registrations**, `(ILjava/lang/CharSequence;)L{class};` and `(ILjava/lang/CharSequence;II)L{class};`, on all three classes |

Fourteen unit tests are added in a new `g26_builder_text_tests` module. They
assert **units**, never a `read_string` round trip — a test written through
`read_string` passes on the broken code, because that conversion is the bug. One
test asserts the difference between `read_string_chars` and `read_string` on the
same object, so a later "simplification" back to `read_string` fails loudly
rather than silently.

---

## 5. What this lane did NOT change, and why

* **`register_phase52_string_buffer` (`lang_string.rs:12637`) is left alone.**
  It is a second `java/lang/StringBuffer` registrar, registered under the
  `Bridge` category, and the `--jdk-only` dump shows it owning **zero** slots —
  `register_string_builder_natives` owns all 61. It contains at least one
  wrong-body row (`append(Ljava/lang/CharSequence;)Ljava/lang/StringBuffer;`
  points at `native_sb_append_string`, which reads a `CharSequence` with
  `read_string` and would answer `"null"`), and Compatible mode may reach it.
  **MEASURED, Compatible mode, `G26Builder2`: 29 divergent rows of 66** — one
  more than `--jdk-only`'s 28. Not repaired here because this lane cannot build
  and cannot show which of the two registrars a Compatible-mode call reaches;
  recorded as a residual with its measurement so the next lane starts from a
  number instead of a hunch.
* **`inet_address.rs` was swept and is clean for this assignment.** It owns
  `Inet4AddressImpl`/`Inet6AddressImpl` resolution only. Nothing in the `inet`
  family performs a lookup, by the fixture's own design.
* **`nio_buffer.rs` was swept and is clean for this assignment.** It is default
  off and registers on `java/nio/HeapByteBuffer`. Its `relative`/`absolute` split
  is the exemplar N3 should copy, not the body `bufslice` reaches.

---

## 6. Where the vectors go next — the measured parts and the predicted whole

**`RJdkBridge1 --only=surrog` is the family attached to this fix.** The oracle
passes it with 22 checks. On the current binary it is RED at check 1:
`StringBuilder.append must carry an UNPAIRED high surrogate through unchanged`.
`scratchpad/g26/src/G26Surrog.java` replays all 22 assertions as observables so
nothing stops there — **8 divergent rows of 28**:

| row | HotSpot | before | reached through |
|---|---|---|---|
| `append(lone).charAt(1)` | 55296 | 65533 | `append(String)` |
| `insert(1, lone).charAt(2)` | 55296 | 65533 | `insert(int, String)` |
| `indexOf(lone unit)` | 2 | **-1** | haystack built by `append(String)` |
| `new SB(lone).codePointAt(1)` | 55296 | 65533 | `<init>(String)` |
| `reverse(lone low).charAt(1)` | 56320 | 65533 | `<init>(String)` |
| `substring(1,2).charAt(0)` | 55296 | 65533 | `<init>(String)` |
| `Properties` value `charAt(2)` | 56320 | 65533 | `"v" + LONE_LO` → `append(String)` |
| `URI.toString().charAt(10)` | 55296 | 65533 | `"http://h/" + LONE_HI` → `append(String)` |

All eight route through the three entry points fixed here — the last two via
string concatenation, which javac compiles to `StringBuilder.append(String)`.
The twenty rows that already match include `codePointCount` on both a lone
surrogate and a well-formed pair, `setCharAt`, the `TreeMap` code-unit ordering
rule, `ArrayDeque`/`Vector` value matching, and `new BigInteger` with a
surrogate among the digits raising a real `NumberFormatException` rather than a
Rust conversion failure.

**PREDICTED from MEASURED parts:** after this fix `RJdkBridge1 --only=surrog`
completes at **22**. The prediction is as strong as G21-1's `Handler.setLevel`
one and for the same reason — the corrected behaviour is already running, on the
same file, on the read side (`read_string_chars`) and on the `toString()` write
side (`sb_string_from_units`), and both are measured correct today.

**`RJdkIntrinsics3` does not move at all from this lane's edits.** Its `misc`
blocker is N4's file. With N1–N4 and N6 applied the four assigned families
complete at 22 + 34 + 21 + 33 = 110 checks, and the vector's remaining known
blocker is `tlocal`'s `InheritableThreadLocal` capture timing, which
HANDOFF-20260814 §6.5 and F41-1 §6 deliberately defer.

---

## 7. The traps this lane hit

* **"All four fall in your files" was a claim about source, and the registry
  disagreed with it in one command.** Five minutes of `--dump-native-registry`
  against each family's own `--only=` workload replaced four hours of editing
  the wrong file. HANDOFF §4's instruction to trust the dump over any comment
  extends to trusting it over the brief.
* **A family's first failing assertion is not its worst row.** `fmtobj` looked
  like a one-slot fix and is a 16-row one; `bufslice` looked like a sweep and is
  a one-body one. Neither shape was visible from the failing assertion, and the
  two guesses would have been wrong in opposite directions.
* **The measurement has to be able to hold the value.** The first version of the
  surrogate probe printed strings; HotSpot's Windows console renders a lone
  surrogate and U+FFFD identically, so the probe reported agreement. Every row
  here prints `charAt` as an integer or a `U+XXXX` list built from
  `Integer.toHexString`. Every printed label is ASCII, per F41-1 §4.
* **A probe that aborts the VM truncates its own table.** `G26Builder` died at
  row 26 of 66 because row i6 panicked the VM; moving that one row to the end
  (`G26Builder2`) recovered the other 40. A crashing row hides everything after
  it, which is the same failure mode `--only=` exists to solve one level up.

---

## 8. NOMINATIONS

`G23-1-the-nominations-that-needed-lib-rs-20260817.md` is a concurrent lane
holding `lib.rs` in this same worktree. It touches `InheritableThreadLocal`,
`Logger.setParent`, `LogRecord` and `alloc_synth_timezone`, and **neither
`java/util/Formatter` nor `ByteBufferAsCharBuffer`** — so N1 and N3 below are
new work for that file, not a duplicate of anything already written there.

**N1 — `native-builtins/src/lib.rs`, `java/util/Formatter`.** Three
constructors (`lib.rs:21574`, `:21594`, `:21604`) write a Java `String` into
slot 0 where the JDK writes `new StringBuilder()`, so `out()` (`:21725`)
returns a `String`; and `close()` (`:21732`) is a no-op, so none of
`toString`/`out`/`flush`/`format`/`locale` throws `FormatterClosedException`
afterwards and a post-close `format()` still appends to the caller's buffer.
16 of 38 rows, table in §2.1. Note the file's own long comment at `:21700`
already anticipates this: it explains that the registrations exist *because*
slot 0 is a `String` that `read_string` can decode, and that fixing it means
also fixing `toString()` — which has since been done. That precondition is met.
`RJdkIntrinsics3 --only=fmtobj` is the gate, 22 checks.

**N2 — `native-builtins/src/phases_early.rs:12953` and `:12865`,
`InetSocketAddress.createUnresolved(String,int)` and `<init>(String,int)`.** A
null hostname must be `IllegalArgumentException("hostname can't be null")`, and
the **port check runs first** — `createUnresolved(null, -1)` is
`port out of range:-1` (no space after the colon). 3 of 51 rows, §2.2.
`RJdkIntrinsics3 --only=inet` is the gate, 34 checks.

**N3 — `native-builtins/src/lib.rs:34972`, `ByteBufferAsCharBuffer{B,L,RB}`.**
The RELATIVE `get()` must throw `java.nio.BufferUnderflowException`; the
absolute `get(int)`, `charAt(int)` and `put(int,char)` keep
`IndexOutOfBoundsException` and are already right, and the relative `put(char)`
already answers `BufferOverflowException`. One body, five measured rows, §2.3.
The working exemplar is `phases_late/nio_buffer.rs`'s `relative::<WIDTH>`.
`RJdkIntrinsics3 --only=bufslice` is the gate, 33 checks.

**N4 — `native-builtins/src/deprecated_util.rs:656`,
`native_string_init_from_string_builder`.** Replace
`String::from_utf16_lossy(&chars).into_bytes()` +
`string_from_bytes_utf8` with `crate::lang_string::sb_string_from_units(ctx,
&chars)`, which is `pub(crate)`, carries the GC pin, and keeps the well-formed
fast path byte-for-byte. This is the `misc` blocker and it is a one-line change.

**N5 — `native-builtins/src/lang_math.rs:771`,
`String.valueOf(Ljava/lang/Object;)`.** Loses a lone surrogate. MEASURED:
`String.valueOf((Object) lone).charAt(0)` is 56320 on HotSpot and 65533 here,
which is also why `String.join("-", lone)` is wrong (the JDK's `join` is
`String.valueOf(elements[i])` per element). `lang_string::invoke_to_string_units`
is now available and is the intended helper.

**N6 — `native-builtins/src/phases_early.rs:6084`,
`java/util/EnumMap.<init>(Ljava/lang/Class;)V`.** A null key type must throw
`NullPointerException`; it returns. The second `misc` blocker, §2.4.

**N7 — `native-builtins/src/phases_late/charset_buffers.rs`, `cb_read_text`.**
It answers a Rust `String`, so the `java/nio/*CharBuffer*` arm of
`charsequence_fast_units` is still lossy for a lone surrogate — the one arm of
that function this lane could not repair. A units-returning sibling would close
it; the call site is commented and points here.

**N8 — G9-1's NOMINATION N4 (`read_string_units` on `NativeContext`) is not
needed and should be closed.** `lang_string::read_string_chars` already is that
function, is layout-aware for the JDK 9+ compact `byte[]`, and is what every
fix in §3 uses. Recorded so the next lane does not add a third spelling of the
same construction — the exact thing `init_string_from_units`' own doc comment
warns against.

---

## 9. What this lane did NOT settle

* **It did not measure its own "after".** `cargo build`/`check`/`test` were
  forbidden — the orchestrator holds the target-dir lock — so the binary predates
  every edit here by construction. Every "after" in §0 is PREDICTED. What is not
  predicted is every "before", and §6's destination, which is measured on the
  mechanism rather than on the fix.
* **It did not run the unit tests it wrote.** Fourteen tests, written against
  the `mock_ctx` / `MockNativeContext` idiom already used by this file's
  `sb_append_charsequence_off_len` tests, with the `NativeHeapAccess` import
  block from `lang_class.rs`'s test module. No lane can claim a passing test it
  did not run.
* **It did not repair `register_phase52_string_buffer`** (§5), though it
  measured Compatible mode at 29 divergent rows against `--jdk-only`'s 28.
* **It did not fix any of the four assigned families**, because none of them is
  in a file this lane owns (§1). They are measured whole and nominated (§8).
* **It did not touch `tlocal` or `logrec`**, per the brief.
* **It did not sweep `String`'s own natives for the same defect.** They are all
  `Bridge`-category in `lang_string.rs` and dropped in `--jdk-only`, so they are
  invisible to this lane's gate; `lang_math.rs`'s `valueOf` (N5) is the one that
  measurably survives, and there may be others in Compatible mode.

---

## 10. Verification performed

* `rustfmt --edition 2021 --check`, run **in place, in its crate**, on all three
  owned files. `lang_string.rs`: **36 diff hunks**, and the baseline —
  `git show HEAD:native-builtins/src/lang_string.rs` into a temp file, checked
  the same way — is **also 36**. `inet_address.rs`: 5, unchanged, untouched.
  `nio_buffer.rs`: 0, unchanged, untouched. **Zero new formatting deviations.**
  The baseline was obtained without `git stash`; no state-changing git command
  was run by this lane.
* Zero CR bytes in every edited file (`tr -cd '\r' < f | wc -c` → 0).
* The appended test module was formatted by extracting it to a standalone `.rs`,
  running `rustfmt` on that, and splicing it back — so the module is
  rustfmt-clean without reflowing the file's 36 hunks of pre-existing debt.
* Re-run on the current binary, both VMs, after the edits (the binary predates
  them, so these are the unchanged baselines the next build is compared against):

| target | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `RJdkIntrinsics3 --only=fmtobj` | PASS 22 | RED, row 1 |
| `RJdkIntrinsics3 --only=inet` | PASS 34 | RED, `createUnresolved(null, 80)` |
| `RJdkIntrinsics3 --only=misc` | PASS 21 | RED, `charAt(0)` = 65533 |
| `RJdkIntrinsics3 --only=bufslice` | PASS 33 | RED, `BE get() at the limit` |
| `RJdkIntrinsics3 --only=regex` | PASS 42 | **PASS 42** |
| `RJdkIntrinsics3 --only=mathexact` | PASS 57 | **PASS 57** |
| `RJdkStringCodePoints` | PASS 186 | **PASS 186** |
| `RStrings` | PASS 46 | **PASS 46** |
| `RJdkNet` | PASS 81 | **PASS 81** |
| `RDirectBufferElem` | PASS 506 | **PASS 506** |
| `RJdkByteOrder` | PASS 84 | **PASS 84** |
| `RJdkFormatLocale` | PASS 20 | **PASS 20** |

* Re-running those twelve against a binary containing the fix is the outstanding
  gate. `RJdkBridge1 --only=surrog` (22) and `RJdkIntrinsics3 --only=misc` are
  the two that should MOVE; the six whole vectors are the blast-radius gate and
  must not.
