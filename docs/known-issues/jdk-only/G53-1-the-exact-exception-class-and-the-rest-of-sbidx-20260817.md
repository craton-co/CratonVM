# G53-1 — the exact exception class, and the rest of `sbidx`

**Status:** FIXED IN SOURCE, **after-state NOT MEASURED and could not be.**
This lane's brief forbids `cargo build`, `check` and `test`. The only binary on
this host is `C:/craton/target-rel4/release/cratonvm.exe`, built from
`cb2ade4fd`, which predates every edit here. Every "after" below is **PREDICTED**
and labelled at each occurrence.

What **is** MEASURED:

* **The whole `getChars`/`sbidx` family on the ORACLE** — Adoptium
  jdk-25.0.3.9-hotspot on this host: 74 rows covering every method the brief
  named, plus messages and check precedence (§1, §2).
* **The same 74 rows on CratonVM** (`cb2ade4fd`) via a probe that uses no
  lambdas and no `invokedynamic`, so the identical class file runs on both VMs
  (§3). **64 of 74 already matched**; the 10 that did not are exactly four
  defects, and nothing else in the family is wrong.
* **The before-state of `RJdkBridge1`** and all six green vectors (§0).
* **Native-registry ownership** for every method touched (§4).

**Owned file:** `native-builtins/src/lang_string.rs`, and nothing else.
Everything else is a NOMINATION in §7.

---

## 0. The headline

| | before (MEASURED, `cb2ade4fd`) | after (PREDICTED) |
|---|---|---|
| `RJdkBridge1` | **236 checks, dies in `sbidx`** | **~301, dies in `surrog`** (§6) |
| `RStrings` | PASS, 46 | unchanged |
| `RJdkStringCodePoints` | PASS, 186 | unchanged |
| `RJdkHello` | PASS, 41 | unchanged |
| `RJdkIntrinsics3` | PASS, 1011 | unchanged |
| `RJdkFormatLocale` | PASS, 20 | unchanged |
| `RCollections` | PASS, 53 | unchanged |

Oracle for `RJdkBridge1` is **394 checks, PASS**. The family layout, MEASURED
with `--list` and `--only=<family>`:

```text
props=40  treenav=42  collect=19  deque=33  vector=20  uri=50  bytebuf=32
sbidx=55  surrog=22  atomarr=26  bigint=55                        total 394
```

`uri=50` reproduced exactly, so the URI blocker really is gone. `atomarr` (26)
and `bigint` (55) **already PASS in isolation** — MEASURED with `--only`. The
tail is therefore two families, not one: `sbidx` (this lane) and `surrog`
(NOMINATION N1/N2, §7).

---

## 1. The trap: `getChars` throws THREE classes, one line apart

`java/lang/AbstractStringBuilder.java:563-573` (jdk-25.0.3.9 `src.zip`) is the
whole story, and it is explicit:

```java
Preconditions.checkFromToIndex(srcBegin, srcEnd, count, Preconditions.SIOOBE_FORMATTER);
int n = srcEnd - srcBegin;
Preconditions.checkFromToIndex(dstBegin, dstBegin + n, dst.length, Preconditions.IOOBE_FORMATTER);
```

Two `checkFromToIndex` calls, two **different** formatters. The source window
gets `StringIndexOutOfBoundsException`; the destination window gets the **plain
`IndexOutOfBoundsException`**.

This is the same shape `HANDOFF-20260814` §5 records for `Vector.subList` — a
method that "keeps the plain exception where every sibling throws the subclass".
Both wrong answers here (`ArrayIndexOutOfBoundsException`, which the element
stores suggest, and `StringIndexOutOfBoundsException`, which the neighbouring
check three lines up throws) are **subclasses** of the right one. A test written
as `catch (IndexOutOfBoundsException)` passes on all three. `RJdkBridge1`
asserts the exact class name and does not.

**`String.getChars` does NOT share this split.** `String.java:1755-1763` uses
`checkBoundsBeginEnd` **and** `checkBoundsOffCount`, and *both* resolve to
`SIOOBE_FORMATTER` (`String.java:4936`). So `"abcde".getChars(0,3,new char[2],0)`
is a `StringIndexOutOfBoundsException` where the builder's is the plain one.
Same method name, same argument list, different contract. **Do not unify them.**
(Moot for this file anyway — §4.)

## 2. The measured table (ORACLE, Adoptium jdk-25.0.3.9-hotspot)

Receiver `"abcde"`, length 5. Message text included because it identifies
*which* window was reported, not just which class.

### `AbstractStringBuilder` / `StringBuilder` / `StringBuffer` — `getChars(int,int,char[],int)`

| call | class | message |
|---|---|---|
| `getChars(0,3,char[2],0)` too-small dst | **`IndexOutOfBoundsException`** | `Range [0, 3) out of bounds for length 2` |
| `getChars(0,3,char[5],3)` `dstBegin+n` overruns | **`IndexOutOfBoundsException`** | `Range [3, 6) out of bounds for length 5` |
| `getChars(0,3,char[5],-1)` negative `dstBegin` | **`IndexOutOfBoundsException`** | `Range [-1, 2) out of bounds for length 5` |
| `getChars(3,1,char[5],0)` `srcBegin > srcEnd` | `StringIndexOutOfBoundsException` | `Range [3, 1) out of bounds for length 5` |
| `getChars(-1,3,char[5],0)` negative `srcBegin` | `StringIndexOutOfBoundsException` | `Range [-1, 3) out of bounds for length 5` |
| `getChars(0,6,char[5],0)` `srcEnd > length()` | `StringIndexOutOfBoundsException` | `Range [0, 6) out of bounds for length 5` |
| `getChars(0,3,null,0)` null dst | `NullPointerException` | `Cannot read the array length because "dst" is null` |
| `getChars(3,1,null,0)` null dst **+ bad src** | **`StringIndexOutOfBoundsException`** | `Range [3, 1) out of bounds for length 5` |
| `getChars(0,3,null,-1)` null dst + bad `dstBegin` | `NullPointerException` | (as above) |
| `getChars(0,0,null,0)` null dst, **empty copy** | **`NullPointerException`** | (as above) |
| `getChars(0,0,char[0],0)` / `(5,5,char[5],5)` / `(0,3,char[3],0)` | *no throw* | — |

`StringBuffer` mirrors `StringBuilder` on every row (MEASURED, not assumed).

### The precedence, which is four checks deep

The last four rows above pin an ordering that is **not** "validate arguments,
then act":

1. **source window first** — so a bad source beats a null `dst`
   (`getChars(3,1,null,0)` is SIOOBE, *not* NPE);
2. **then `dst.length` is read** — so a null `dst` throws **even when `n == 0`**
   and nothing would be copied (`getChars(0,0,null,0)` throws);
3. **then the destination window**, with the plain `IndexOutOfBoundsException`.

Checking the null argument up front — the obvious way to write it, and the way
this file did — gets rows 1 and 2 of that list wrong in opposite directions.

### `String.getChars` — the same calls, a different answer

| call | class |
|---|---|
| `"abcde".getChars(0,3,char[2],0)` | **`StringIndexOutOfBoundsException`** |
| `"abcde".getChars(0,3,char[5],-1)` | `StringIndexOutOfBoundsException` |
| `"abcde".getChars(3,1,char[5],0)` | `StringIndexOutOfBoundsException` |
| `"abcde".getChars(0,3,null,0)` | `NullPointerException` |
| `"abcde".getChars(3,1,null,0)` | `StringIndexOutOfBoundsException` |

Every bounds failure is the String subclass. Confirms §1: the two `getChars`
are different contracts.

### `codePointCount` — the one row with a **null** formatter

`AbstractStringBuilder.java:525` is
`Preconditions.checkFromToIndex(beginIndex, endIndex, count, null)`. A null
formatter yields the plain `IndexOutOfBoundsException`:

| call | class | message |
|---|---|---|
| `codePointCount(0,6)` | **`IndexOutOfBoundsException`** | `Range [0, 6) out of bounds for length 5` |
| `codePointCount(-1,3)` | **`IndexOutOfBoundsException`** | `Range [-1, 3) out of bounds for length 5` |
| `codePointCount(3,1)` | **`IndexOutOfBoundsException`** | `Range [3, 1) out of bounds for length 5` |

Its neighbours `delete` / `replace` / `substring` are `SIOOBE_FORMATTER`
(`:1026`, `:1113`, `:1189`) — all three MEASURED as
`StringIndexOutOfBoundsException`. In this file all four went through **one
shared helper**, `sb_check_from_to_index`, which throws SIOOBE. So the fix is
*not* in the helper; changing it would break three correct methods to fix one.

### The neighbouring `sbidx` rows the vector already passes

All MEASURED identical on both VMs before and unaffected by this lane's change
(§3): `charAt(-1)`, `charAt(5)`, `deleteCharAt(±)`, `delete(3,1)`,
`delete(-1,2)`, `setLength(-1)`, `setCharAt(-1)`, `setCharAt(len)`,
`insert(len+1,x)`, `insert(-1,x)`, `substring(3,1)`, `substring(len+1)`,
`codePointAt(len)`, `codePointBefore(0)`, `replace(3,1,z)`, and the four
`StringBuffer` rows — every one `StringIndexOutOfBoundsException`;
`appendCodePoint(-1)` / `appendCodePoint(0x110000)` / `repeat(cs,-1)` —
`IllegalArgumentException`.

### The other `IOOBE_FORMATTER` methods in the class

Swept because they are the same defect class. **All three already correct in
this file** — no change made:

| call | class | already right? |
|---|---|---|
| `append(CharSequence,int,int)` out of range | `IndexOutOfBoundsException` | yes |
| `append(char[],int,int)` out of range | `IndexOutOfBoundsException` | yes |
| `insert(int,CharSequence,int,int)` out of range | `IndexOutOfBoundsException` | yes |
| `insert(int,char[],int,int)` out of range | `StringIndexOutOfBoundsException` (`:1228`) | yes |

## 3. The before-state on CratonVM, and the four defects

Probe: `scratchpad/Probe3.java` + `Probe4.java` — no lambdas, no
`invokedynamic`, so one class file runs on both VMs. 74 rows.

**64 of 74 matched.** The 10 that diverged are exactly four defects:

| # | rows | MEASURED before (`cb2ade4fd`) | ORACLE | site |
|---|---|---|---|---|
| **A** | 4 | `ArrayIndexOutOfBoundsException` | `IndexOutOfBoundsException` | `native_sb_get_chars` destination check |
| **B** | 1 | `NullPointerException` | `StringIndexOutOfBoundsException` | `native_sb_get_chars` check ORDER |
| **C** | 4 | `StringIndexOutOfBoundsException` | `IndexOutOfBoundsException` | `native_sb_code_point_count` |
| **D** | 1 | *no throw* | `NegativeArraySizeException` | `native_sb_init_capacity` |

Defect **D** was not in the brief and is not a wrong exception class — it is a
**missing** one. `native_sb_init_capacity` clamped with
`std::cmp::max(*v, 0) as usize`, so `new StringBuilder(-1)` silently built a
usable empty builder and `.length()` answered `0`. The JDK constructor body is
just the allocation `value = new byte[capacity]`, so a negative argument fails in
`anewarray`: `NegativeArraySizeException` whose message is the raw size
(MEASURED: `-1` and `-7`). `RJdkBridge1` asserts this **immediately after**
`setLength(-1)`, precisely because the two negative-length paths in this class
deliberately throw *different* classes:

```java
"new StringBuilder(-1) must throw NegativeArraySizeException — a DIFFERENT class
 from setLength(-1)'s; got " + nameOf(t)
```

A `--only=sbidx` run alone would have surfaced D only after A/B/C were fixed and
the vector re-run three times. The probe found all four in one pass, which is
what made a no-rebuild lane viable at all.

**Nothing else in the family is wrong.** Every value row — surrogate-pair
`reverse`, lone-surrogate `appendCodePoint`, `repeat`, `trimToSize`,
`indexOf`/`lastIndexOf` clamping, `delete` end-clamping, `insert(i,(String)null)`
inserting `"null"`, `append((CharSequence)null,0,2)` — matched the oracle
**in UTF-16 units**, before and after.

## 4. Ownership, established before writing

`--dump-native-registry` under `--jdk-only` (schema 4). This file owns **191
slots** — `AbstractStringBuilder` 63, `StringBuffer` 63, `StringBuilder` 63,
`StringUTF16` 2 — and nothing else.

| method | class(es) | `owns_slot` | registered by |
|---|---|---|---|
| `getChars(II[CI)V` | ASB, `StringBuilder`, `StringBuffer` | **true** | `lang_string.rs:341` |
| `codePointCount(II)I` | ASB, `StringBuilder`, `StringBuffer` | **true** | `lang_string.rs:327` |
| `<init>(I)V` | ASB, `StringBuilder`, `StringBuffer` | **true** | `lang_string.rs:172` |

**`java/lang/String.getChars` has NO native registration at all** — it is absent
from the census, so real JDK bytecode runs and §1's String-side contract is
served correctly without this file. Of the 27 `java/lang/String` rows, none is
`getChars`. This is why §1's warning is a warning and not a second fix.

`invocations` was `0` for all three, as the brief predicted it would be; it
proves nothing and was not used.

## 5. The fix

All three in `native-builtins/src/lang_string.rs`:

1. **`native_sb_get_chars`** — destination check now
   `RuntimeError::ioobe(out_of_bounds_message::check_from_to_index(dstBegin,
   dstBegin+n, dst.length))`, the plain class with the destination window's
   wording; and the body is **reordered** to source-check → `dst` null-deref →
   destination-check, so the §2 precedence holds. The NPE message now matches
   HotSpot's helpful-NPE text.
2. **`native_sb_code_point_count`** — the range check is **inlined** rather than
   delegated to `sb_check_from_to_index`, so it can throw the plain
   `IndexOutOfBoundsException` while `delete` / `replace` / `substring` keep the
   shared SIOOBE helper untouched.
3. **`native_sb_init_capacity`** — a negative capacity now throws
   `NegativeArraySizeException { size }` instead of clamping to `0`. The check
   precedes the handle scope, so no allocation is attempted.

Tests, in the existing `#[cfg(test)] mod tests`, all asserting **UTF-16 units**
and exact classes via `err_kind` (which already discriminated
`ioobe`/`aioobe`/`sioobe` — a `matches!` on `IndexOutOfBoundsException` cannot,
because both wrong answers are subclasses):

* `sb_get_chars_too_small_dst_throws_the_plain_ioobe_not_a_subclass` — the
  vector's row verbatim, plus the message, to catch reporting the *source*
  window with the right class;
* `sb_get_chars_negative_dst_begin_throws_the_plain_ioobe`,
  `sb_get_chars_dst_overrun_throws_the_plain_ioobe` — **rewritten**; these two
  previously asserted `"aioobe"` and encoded the defect;
* `sb_get_chars_bad_src_range_beats_a_null_destination`,
  `sb_get_chars_null_dst_throws_npe_even_when_nothing_would_be_copied` — the
  §2 precedence, both directions;
* `sb_code_point_count_bad_range_throws_the_plain_ioobe` (3 shapes) and
  `sb_substring_and_delete_keep_the_string_subclass_that_code_point_count_drops`
  — the paired assertion, so "fixing" this in the shared helper fails loudly;
* `sb_code_point_count_valid_window_still_counts_surrogate_pairs`;
* `sb_init_capacity_negative_throws_negative_array_size_not_a_clamp`,
  `sb_init_capacity_zero_and_positive_still_build_an_empty_builder`.

## 6. Where `RJdkBridge1` lands (PREDICTED)

`sbidx` is 55 checks. Its rows are a subset of the 74 probed in §3; after A–D
all 74 match the oracle, so **`sbidx` should go green and the vector should
reach ~301** (236 + 55 + the ~10 `surrog` rows that already pass).

The next divergence is then **`surrog`**, MEASURED today with `--only=surrog` on
`cb2ade4fd` — it is **not** in this file:

```text
CK RJdkBridge1 surrog-step=Properties.setProperty(lone key, lone value)
AssertionError: the value must come back with its unpaired low surrogate intact
```

Isolated (`scratchpad/Props.java`), MEASURED both VMs:

```text
                        ORACLE                    CRATONVM
key   "k" + a\uD800b    len=4 [6b,61,d800,62]     len=4 [6b,61,d800,62]   same
value "v" + a\uDC00b    len=4 [76,61,dc00,62]     len=4 [76,61,dc00,62]   same
p.getProperty(key)      len=4 [76,61,dc00,62]     len=4 [76,61,fffd,62]   U+FFFD
HashMap.get(key)        len=4 [76,61,dc00,62]     null                    LOST
```

The key round-trips intact but the **value** comes back with its unpaired low
surrogate replaced by U+FFFD — a Rust `str` round-trip. The `HashMap` row is
worse and is a **separate** defect: an equal lone-surrogate key does not even
find its entry. Both are NOMINATIONS (§7). `surrog` is 22 checks, so the vector
cannot go green on this lane's file alone.

## 7. NOMINATIONS

**N1 — `Properties.getProperty` replaces an unpaired surrogate with U+FFFD.**
Owner: the `java/util/Properties` natives (`properties_sidetable.rs` per the
registry). Evidence §6. Blocks `RJdkBridge1` `surrog`. The value is stored and
retrieved through a Rust `str`, which cannot hold an unpaired UTF-16 surrogate;
it needs the units-preserving path the rest of `surrog` already relies on.

**N2 — `HashMap.get` misses a key containing an unpaired surrogate.**
Returns `null` for a key that is `equals` to the stored one (§6). Strictly worse
than N1 — silent lookup failure, not corruption. Not exercised by `surrog`'s
current rows (which use `Properties`/`TreeMap`/`ArrayDeque`/`Vector`), so it is
latent, but it is the same root cause and should move with N1.

Neither is in `native-builtins/src/lang_string.rs`. No edits were made outside
that file.

## 8. What I could not settle

* **The after-state.** No `cargo build`/`check`/`test` is permitted and no
  binary newer than `cb2ade4fd` exists on this host, so §0's "after" column and
  §6 are PREDICTED from the oracle table and the source diff, not observed. The
  prediction is falsifiable and cheap to check: re-run `--only=sbidx`; it should
  report `sbidx=55` and PASS.
* **The unit tests have not been compiled.** They are written against symbols
  verified by inspection (`err_kind` already had an `ioobe` arm;
  `try_new_array` defaults to the infallible `new_array`, so `mock_ctx`
  satisfies `native_sb_init_capacity`; `alloc_object` is on `NativeHeapAccess`,
  already in the module's import block). That is not the same as a green
  `cargo test` and is not claimed to be.
* **Whether `surrog`'s remaining rows hide further defects past the
  `Properties` one.** The family aborts at that assertion, so rows after it
  (`TreeMap`, `ArrayDeque`, `Vector`, `URI`, `BigInteger`, `new String(char[])`
  with lone surrogates) are unmeasured. `surrog` is 22 checks and only ~10 are
  known to pass.
