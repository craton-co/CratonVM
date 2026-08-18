# E8-1 — `String`'s reference arguments swallowed every null, and the six version-skew constants needed a third copy

**Status: FIXED in `native-builtins/src/lang_string.rs` and
`native-builtins/src/case_map.rs` (lane E8, 2026-08-13). Both files are owned
by this lane; nothing outside them was touched. UNBUILT and UNRUN — this lane
may not run `cargo` or the CratonVM binary, so every CratonVM "after" below is
marked PREDICTED. The HotSpot columns are measured, with transcripts.**

Oracle: Microsoft OpenJDK 25.0.3+9 (`java -version` confirmed in-session).
Probes: `scratchpad/e8/NullContracts.java`, `scratchpad/e8/SigmaCased.java`,
`scratchpad/e8/Residuals.java`.

---

## 1. The reported failure, and what it actually was

`RJdkIntrinsics2 --only=bounds` failed on the current binary with

```
String.regionMatches with a null other must throw NullPointerException
```

The obvious reading — "add a null check at the top of the native" — is
**wrong**, and the fixture's own message says why: the negative-length row a
previous lane pinned (`"ABC".regionMatches(0, "abc", 0, -1)` is `true`) proves
`len` is *not* validated before `other` is used, so the two rules interact.

The JDK's bounds test is a four-term short-circuiting `||`, and `other` is
dereferenced by the **fourth** term:

```java
if ((ooffset < 0) || (toffset < 0)
        || (toffset > (long) length() - len)
        || (ooffset > (long) other.length() - len)) {
    return false;
}
```

So a null `other` is sometimes an NPE and sometimes a plain `false`. Measured
on OpenJDK 25.0.3+9, receiver `"abc"` unless noted:

| call | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `regionMatches(0, null, 0, 1)` | **NullPointerException** | `false` |
| `regionMatches(0, null, 0, -1)` | **NullPointerException** | `false` |
| `regionMatches(3, null, 0, 0)` | **NullPointerException** | `false` |
| `"".regionMatches(0, null, 0, 0)` | **NullPointerException** | `false` |
| `regionMatches(-1, null, 0, 1)` | `false` | `false` |
| `regionMatches(0, null, -1, 1)` | `false` | `false` |
| `regionMatches(99, null, 0, 1)` | `false` | `false` |
| `regionMatches(0, null, 0, 4)` | `false` | `false` |

and the five-argument overload answers identically for `ignoreCase = true`,
delegating to the above for `ignoreCase = false` (`regionMatches(false, 99,
null, 0, 1)` is `false`, `regionMatches(false, 0, null, 0, 1)` throws).

A null check written at the top of the native would turn four of those eight
rows red. **The order is the contract**, so the fix transcribes the expression
term for term: `region_matches_impl` now takes
`other: Option<ObjectRef>`, evaluates the first three terms against the
receiver's length alone, and only then unwraps — the unwrap IS the JDK's
implicit null check. The first three terms are split into a pure
`region_matches_short_circuits(this_len, toffset, ooffset, len) -> bool` so
that the short-circuit point is a unit-testable fact and not a comment; all
eight rows above are pinned in
`region_matches_dereferences_other_exactly_where_the_jdk_does`.

**PREDICTED: the one `--only=bounds` check at
`regression-suite/src/RJdkIntrinsics2.java:1406` flips fail → pass. Section
count 35, no other row in the section touches these files.**

---

## 2. The sweep: `String`'s reference arguments, and why "the same shape" is
   not the same answer

The failing row is one instance of a family. `String`'s natives are written

```rust
let other = match args.get(1) {
    Some(Value::Object(Some(obj))) => *obj,
    _ => return Ok(Some(Value::Int(0))),   // <- a null takes the failure exit
};
```

and that `_` arm quietly turns a contract violation into whatever the failure
value happens to be. Swept across `lang_string.rs`, the swallowed nulls were
returning `false` (predicates), `-1` (searches, indistinguishable from a real
miss), a **NULL `String[]`** (`split` — which then NPEs at the caller's line,
blaming the caller), `""` (`join`, `copyValueOf` — and `""` is a legal result
of both), the receiver unchanged (`transform`, i.e. the identity function), and
a silent successful return (`getChars`).

**The contracts are not uniform, so every row below was measured rather than
inferred from the parameter type.** The full transcript is in
`scratchpad/e8/NullContracts.java`; the shape summary:

| method | HotSpot 25 | CratonVM (before) | after (PREDICTED) |
|---|---|---|---|
| `equals(null)` | `false` | `false` | unchanged |
| `equalsIgnoreCase(null)` | `false` | `false` | unchanged |
| `startsWith(null, -1)` | `false` | `false` | unchanged |
| `String.valueOf((Object) null)` | `"null"` | `"null"` | unchanged |
| `String.join(",", "a", null, "b")` | `"a,null,b"` | `"a,null,b"` | unchanged |
| `String.format("%s", (Object[]) null)` | `"null"` | `"null"` | unchanged |
| `contains(null)` | NPE | `false` | NPE |
| `startsWith(null)` / `startsWith(null, 0)` / `(null, MAX)` | NPE | `false` | NPE |
| `endsWith(null)` | NPE | `false` | NPE |
| `indexOf((String) null)` | NPE | `-1` | NPE |
| `lastIndexOf((String) null)` | NPE | `-1` | NPE |
| `compareToIgnoreCase(null)` | NPE | `0` ("equal") | NPE |
| `split(null)` / `split(null, 2)` | NPE | **null `String[]`** | NPE |
| `String.join(null, …)` | NPE | `""` | NPE |
| `String.join(",", (CharSequence[]) null)` | NPE | `""` | NPE |
| `String.join(",", (Iterable) null)` | NPE (no message) | `""` | NPE |
| `String.copyValueOf(null)` / `valueOf((char[]) null)` | NPE | `""` | NPE |
| `getChars(0, 1, null, 0)` | NPE | silent no-op | NPE |
| `transform(null)` | NPE | returned `this` | NPE |
| `toUpperCase((Locale) null)` / `toLowerCase((Locale) null)` | NPE | default-locale result | NPE |
| `compareTo(null)`, `concat(null)`, `replace(null, x)`, `matches(null)`, `replaceAll`/`replaceFirst(null, …)` | NPE | NPE | already correct |

The two `false` rows and the three rendering rows are the point: **"takes a
reference parameter" does not imply "throws"**, and `equals` /
`equalsIgnoreCase` in particular are specified to answer `false`. A rule
applied by shape would have broken them.

### 2.1 Two rows where the null contract is conditional

`startsWith(String, int)` has `regionMatches`'s structure —
`if (toffset < 0 || toffset > length() - prefix.length()) return false;` — so
`startsWith(null, -1)` is `false` and every other offset throws. Measured, all
four rows. The old body also read `toffset` as `*v as usize`, which
reinterprets `-1` as `usize::MAX`; it reached the right answer by accident,
from an expression that would have been a panic had the comparison been
written the other way round.

`getChars` has **three** checks in a fixed order, and had none of them:

```text
getChars(0, 9, null, 0)          StringIndexOutOfBoundsException: Range [0, 9) out of bounds for length 3
getChars(2, 1, null, 0)          StringIndexOutOfBoundsException: Range [2, 1) out of bounds for length 3
getChars(-1, 1, null, 0)         StringIndexOutOfBoundsException: Range [-1, 1) out of bounds for length 3
getChars(0, 1, null, 0)          NullPointerException: Cannot read the array length because "dst" is null
getChars(0, 1, null, -1)         NullPointerException                       <- null beats dstBegin
getChars(0, 1, new char[4], -1)  StringIndexOutOfBoundsException: Range [-1, -1 + 1) out of bounds for length 4
getChars(0, 3, new char[4], 2)   StringIndexOutOfBoundsException: Range [2, 2 + 3) out of bounds for length 4
getChars(0, 0, new char[0], 0)   (returns normally)
```

The source range is checked first and fires even when `dst` is null; both range
failures are `StringIndexOutOfBoundsException`, **not**
`ArrayIndexOutOfBoundsException`, including the one about the destination
array. The old body clamped `srcEnd` to the receiver's length, ignored a
negative `srcBegin`, and no-op'd on a null `dst` — so a caller that mis-sized
its buffer got a partly-filled array and no signal at all.

**This is the highest-regression-risk edit in the patch**, because `getChars`
is a bulk primitive with many in-tree callers: anything that was relying on the
silent clamp now throws. That is the correct contract (HotSpot throws for the
same inputs), but it converts silent wrongness into a visible exception, which
is what a regression looks like from the outside. `getChars` also now reads its
source through `read_string_chars` rather than
`ctx.read_string(..).encode_utf16()`, so the length the check uses is the same
length `String.length()` reports and unpaired surrogates copy out intact.

### 2.2 `toUpperCase(null)` / `toLowerCase(null)`, and the arity that separates them

`"abc".toUpperCase()` is `"ABC"`; `"abc".toUpperCase((Locale) null)` throws
(from `StringLatin1.toLowerCase`'s `locale.getLanguage()` — which is why
`"".toUpperCase(null)` throws too, there is no empty-string short circuit).
CratonVM answered the default-locale result for both, because `locale_arg`
collapses "absent" and "null" into `Option::None`.

The two ARE distinguishable at the native boundary — the overloads have
different descriptors, so an absent slot and a present-but-null slot are
different `args` lengths — and `locale_arg_checked` now uses that.
`locale_arg` is kept for `jit_string_to_lower_case`, which is handed an
`Option` and genuinely cannot tell them apart (see §5).

One consequential knock-on: `native_string_latin1_to_lower_case` used to
manufacture `Value::Object(None)` for a missing `args[2]` before delegating.
Under the new arity rule that would have made every short call an NPE, so it
now forwards a present slot unchanged and omits an absent one.

---

## 3. `equalsIgnoreCase` / `compareToIgnoreCase`: the doc claimed a fix the code did not have

`case_map::JDK_UNMAPPED_CASE_CODE_POINTS`'s doc (formerly in `lang_string.rs`)
says the six code points were measured and corrected "for
`String.regionMatches(true,…)` / `equalsIgnoreCase` — all four measured". The
measurement was real. The *code* went into `java_char_to_upper_case`, which
`equalsIgnoreCase` never called: it was `a.to_lowercase() == b.to_lowercase()`,
Rust's full mappings over whole strings. `[1 of 10 callsites]` — the helper
existed, the doc described the bug, and one caller used it.

Measured on OpenJDK 25.0.3+9:

| call | HotSpot 25 | `to_lowercase()` gave |
|---|---|---|
| `"İ".equalsIgnoreCase("i")` | `true` | `false` |
| `"K".equalsIgnoreCase("k")` | `true` | `true` |
| `"ǅ".equalsIgnoreCase("Ǆ")` | `true` | `true` |
| `"ß".equalsIgnoreCase("ss")` | `false` | `false` |
| `"ꟓ".equalsIgnoreCase("꟒")` | **`false`** | `true` |
| `"ꟕ".equalsIgnoreCase("꟔")` | **`false`** | `true` |
| `"꟏".equalsIgnoreCase("꟎")` | **`false`** | `true` |
| `"ꟑ".equalsIgnoreCase("Ꟑ")` (control) | `true` | `true` |

Both methods now use the per-code-unit `StringUTF16.regionMatchesCI` rule
already transcribed in `code_unit_eq_ignore_case`, over the thread-local
scratch buffers so a hot path (HTTP header matching) does not gain two
allocations to gain its correctness.

`compareToIgnoreCase` additionally returned a **sign** where the JDK returns a
**difference**: `"_".compareToIgnoreCase("a")` is `-2` on HotSpot (the fold
leaves `_` alone and lowercases `A` back to `a`, so it is `0x5F - 0x61`) and
was `-1`; `"İ".compareToIgnoreCase("i")` is `0` and was non-zero;
`"ꟓ".compareToIgnoreCase("꟒")` is `1` and was `0`. The last two are
sign errors, not magnitude quibbles.

---

## 4. The second task: the six constants had a third home, and the proxy-oracle trap repeated

`lang_string.rs` guarded `String.to{Upper,Lower}Case` against the six
version-skew code points. `case_map.rs` did not, and `string_case_impl` tests
`is_locale_dependent` **first** — so `tr`, `az` and `lt` were handed straight
to the unguarded module. `"ꟓ".toUpperCase(Locale.ROOT)` was right;
`"ꟓ".toUpperCase(Locale.forLanguageTag("tr"))` was wrong. A constant that
lives in one of two files is a fix that half the locales never get.

Line numbers verified before editing, all four confirmed:

| site (pre-edit) | what it was | now |
|---|---|---|
| `case_map.rs:118` | `s.to_lowercase()` — the non-locale-dependent entry | `jdk_to_lowercase(s)` |
| `case_map.rs:126` | `s.to_uppercase()` | `jdk_to_uppercase(s)` |
| `case_map.rs:141-142` | `map_locale_dependent`'s two fallback arms | `push_jdk_lower` / `push_jdk_upper` |
| `case_map.rs:283-284` | `is_cased`'s "does this character have case" test | version-skew arm, see below |

Plus a **fifth** site the brief did not name: `in_word` (`case_map.rs:270`),
the word-boundary approximation `is_final_cased` uses, which asks Rust's
`is_alphanumeric` where the JDK asks `Character.isLetterOrDigit`.

Rather than a third copy, `JDK_UNMAPPED_CASE_CODE_POINTS`, its predicate, and
new `push_jdk_{upper,lower}` / `jdk_to_{upper,lower}case` helpers now live in
`case_map.rs` — next to the table they correct — and `lang_string.rs` imports
the predicate and calls the helpers. `string_case_impl`'s hand-written
character loop is gone; it and `map_locale_dependent`'s arms are now the same
code.

### 4.1 The two shapes need the same arm for the MAPPING and different arms for the PROPERTY

The brief's "both shapes need the same arm" is right for `to{Upper,Lower}Case`
and wrong for `is_cased`, and the difference is measurable. Of the six, four
are UNASSIGNED in the JDK's Unicode 16 (`isDefined false`, `getType 0`) while
`A7D3` (DOUBLE THORN) and `A7D5` (DOUBLE WYNN) are assigned **lowercase
letters** with no uppercase partner. All six map to themselves — same arm — but
`Final_Cased` sees the two groups differently.

Measured through the only observable that reaches `is_cased`, lowercasing
`"A" + U+03A3 + X` (final sigma `U+03C2` vs medial `U+03C3`), identical for
`Locale.ROOT`, `tr` and `lt`:

| X | `Character.isLetterOrDigit` | result | reading |
|---|---|---|---|
| `U+A7CE` `U+A7CF` `U+A7D2` `U+A7D4` | `false` | `a` `03C2` X | **uncased**, sigma is final |
| `U+A7D3` `U+A7D5` | `true` | `a` `03C3` X | **cased**, sigma is medial |
| `U+A7D0` `U+A7D1` (control) | `true` | `a` `03C3` X | cased |
| `a`, `A` (control) | `true` | `a` `03C3` `a` | cased |
| space, `.`, digit (control) | — | `a` `03C2` X | uncased / not in word |

So `is_cased` and `in_word` take `JDK_UNASSIGNED_CASE_CODE_POINTS` — the four —
and not the six. Blanket-excluding all six there would have traded one wrong
answer for another. Note also *where* `is_cased` was wrong: Rust classifies
`A7CE`/`A7D2`/`A7D4` as uppercase letters and `A7CF` as a lowercase one, so the
`c.is_uppercase() || c.is_lowercase()` line answered `true` and the
`is_alphabetic()` clause at `:283-284` was never even consulted for them.

`JDK_UNASSIGNED_CASE_CODE_POINTS` is deliberately **not** a general "is this
assigned in the JDK" predicate — that question is thousands of code points wide
and cannot be answered from Rust's tables at all. It is exactly the four this
file already had to enumerate, extended to the properties the same four get
wrong.

### 4.2 The proxy-oracle warning, honoured

The prior lane's "exhaustive" 65,536-code-unit sweep missed these six because
it diffed the JDK against **Python's** `str.upper()`, and Python here is on UCD
16.0.0 and agrees with the JDK at all six. An exhaustive sweep against a proxy
oracle is still exhaustive.

**This lane ran no sweep.** Every claim above has the JDK on one axis and the
CratonVM *source* on the other — the Rust behaviour is read off
`char::to_uppercase`'s documented contract and off which branch of the Rust
code the value takes, not off a stand-in language. Where the Rust side is
asserted (that Rust pairs `A7CF`/`A7CE`, `A7D3`/`A7D2`, `A7D5`/`A7D4`), it is
the brief's measurement carried forward, not a re-derivation from a third
implementation. The new `case_map.rs` tests are written as JDK-answer
assertions, so if Rust's tables move again they fail loudly rather than
silently agreeing with the wrong oracle.

---

## 5. Residuals this lane did NOT fix

1. **`jit_string_to_lower_case`** (`lang_string.rs`) takes
   `locale: Option<ObjectRef>` and calls `string_case_impl` directly, so it
   bypasses the new null-Locale check. A JIT-compiled
   `s.toLowerCase((Locale) null)` will still answer the default-locale result
   where the interpreter now throws. Fixing it means changing the helper's
   signature, which is a `vm/src/jit/helpers.rs` question — outside this lane,
   and `vm/src/jit/helpers.rs` is being edited by another lane right now.
   `[JIT copi]`.
2. **`String.getBytes(String charsetName)`** in `phases_early.rs` ignores the
   charset entirely and returns the receiver's UTF-8 bytes. Null charset name,
   wrong charset name and `UnsupportedEncodingException` are all unhandled.
   Not this lane's file.
3. **`String.contentEquals`** has no native at all (real bytecode runs), so it
   was out of scope — its contract is recorded here only because the brief
   asked for it: `contentEquals((CharSequence) null)` and
   `contentEquals((StringBuffer) null)` both throw NPE, unlike `equals`.

---

## 6. What should flip

**`--only=bounds`** — exactly one row:

* `RJdkIntrinsics2.java:1406`, `String.regionMatches with a null other must
  throw NullPointerException` — fail → **pass** (PREDICTED). Section total 35.
* Nothing else in the section touches `lang_string.rs` or `case_map.rs`. The
  `AtomicReferenceArray`, `CharBuffer`, `ByteBufferAsCharBuffer` and
  code-point rows are other lanes' files.

**`--only=strfmt`** — **no row should flip.** This is the negative control, and
it is worth stating as such: `strfmt` exercises `String.format`, `formatted`,
`lines`, `indent`, `chars`, `repeat`, `replace`, `replaceAll`, `replaceFirst`,
`matches`, `String.valueOf((Object) null)` and `transform` — and every one of
those calls passes a **non-null** argument, except
`String.valueOf((Object) null)` (specified `"null"`, unchanged) and
`String.format("%s", (Object) null)` (specified `"null"`, unchanged). Section
total 60. **If a `strfmt` row moves, this patch has a bug in it**, most likely
in `transform` or in the shared case-mapping helper.

Nothing in this patch changes any answer for a non-null argument except:

* `equalsIgnoreCase` / `compareToIgnoreCase`, deliberately (§3);
* `String.to{Upper,Lower}Case(Locale)` for `tr`/`az`/`lt` strings containing
  one of the six code points, deliberately (§4);
* `getChars` for out-of-range indices, deliberately (§2.1) — previously
  clamped, now throws.

---

## 7. NOMINATIONS

Two, both against `regression-suite/src/RJdkIntrinsics2.java`, which this lane
does not own. Neither is required for the reported failure to go green; both
close rows the sweep found and nothing currently covers.

### N1 — cover the `regionMatches` short-circuit, not just the one NPE

The fixture pins `regionMatches(0, null, 0, 1)` throws. It does not pin that
`regionMatches(99, null, 0, 1)` answers `false`, which is the half of the
contract a naive "check null first" fix breaks. Two extra rows make the
fixture able to reject that fix.

In `bounds()`, insert immediately **after** the existing block that ends:

```java
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "String.regionMatches with a null other must throw NullPointerException — the"
                        + " NEGATIVE-length row W7-95 pinned returns true, so this one proves the"
                        + " null check happens FIRST; got " + nameOf(t));
```

insert:

```java
        // ... and the other half of the same expression: the JDK's four-term
        // `||` SHORT-CIRCUITS, so `other` is only dereferenced by the fourth
        // term. A null `other` behind a failing earlier term is a plain
        // `false`, NOT a throw — measured on OpenJDK 25.0.3+9. A fix that
        // checks null first passes the row above and fails these two.
        step("bounds", "String.regionMatches(bad toffset, null)");
        t = null;
        boolean shortCircuited = false;
        try {
            shortCircuited = !"ab".regionMatches(OPAQUE_I[13], null, OPAQUE_I[3], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && shortCircuited,
                "\"ab\".regionMatches(9, null, 0, 1) must answer FALSE without touching `other` —"
                        + " term three (toffset > length() - len) decides it; got " + nameOf(t));
        step("bounds", "String.regionMatches(negative ooffset, null)");
        t = null;
        shortCircuited = false;
        try {
            shortCircuited = !"ab".regionMatches(OPAQUE_I[3], null, OPAQUE_I[2], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && shortCircuited,
                "\"ab\".regionMatches(0, null, -1, 1) must answer FALSE — term one (ooffset < 0)"
                        + " decides it before `other` is read; got " + nameOf(t));
```

and change the section total, replacing exactly:

```java
        sectionEnd("bounds", 35);
```

with:

```java
        sectionEnd("bounds", 37);
```

(Uses the existing `OPAQUE_I` entries — `[2] == -1`, `[3] == 0`, `[4] == 1`,
`[13] == 9` — read off the array declaration at
`RJdkIntrinsics2.java:140-142` and confirmed, not inferred from call sites.)

### N2 — the swallowed-null family has no fixture row at all

Nine `String` methods returned a plausible wrong value for a null argument and
no regression row noticed, because every row in `strfmt` and `bounds` passes
valid arguments. The rows below are all measured on OpenJDK 25.0.3+9 and
include the two that must **not** throw, which is the part that makes the block
worth having.

Add a new section method after `bounds()`:

```java
    // -----------------------------------------------------------------------
    // 9b. strnull — the NULL-ARGUMENT contract of String's reference-taking
    //     methods. E8 measured all of these against OpenJDK 25.0.3+9 and found
    //     nine natives answering a plausible wrong VALUE instead of throwing:
    //     `false` from the predicates, `-1` from the searches (indistinguishable
    //     from a real miss), a NULL String[] from split, "" from join and
    //     copyValueOf, the receiver from transform, a silent success from
    //     getChars.
    //
    //     The two `false` rows and the two rendering rows are deliberate and
    //     load-bearing: "takes a reference" does NOT imply "throws", and a fix
    //     applied by shape breaks equals/equalsIgnoreCase. Keep them.
    // -----------------------------------------------------------------------
    static void strnull() {
        String s = "abc";
        String nul = (String) NULL_OBJ;

        // The four that must NOT throw.
        check(!s.equals(NULL_OBJ), "\"abc\".equals(null) must be FALSE, not a throw");
        check(!s.equalsIgnoreCase(nul),
                "\"abc\".equalsIgnoreCase(null) must be FALSE, not a throw — the sibling"
                        + " compareToIgnoreCase(null) DOES throw");
        check("null".equals(String.valueOf(NULL_OBJ)),
                "String.valueOf((Object) null) must be the four-character string \"null\"");
        check("a,null,b".equals(String.join(",", "a", null, "b")),
                "a null ELEMENT of String.join must render as \"null\"");

        // The negative-offset escape hatch: startsWith has regionMatches's
        // short-circuiting shape, so this one is FALSE and not a throw.
        Throwable t = null;
        boolean neg = false;
        try {
            neg = !s.startsWith(nul, OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && neg,
                "\"abc\".startsWith(null, -1) must be FALSE — toffset < 0 is checked before the"
                        + " prefix is dereferenced; got " + nameOf(t));

        // Everything else throws NullPointerException.
        checkNpe("contains", () -> s.contains(nul));
        checkNpe("startsWith", () -> s.startsWith(nul));
        checkNpe("startsWith(_,0)", () -> s.startsWith(nul, OPAQUE_I[3]));
        checkNpe("endsWith", () -> s.endsWith(nul));
        checkNpe("indexOf(String)", () -> s.indexOf(nul));
        checkNpe("lastIndexOf(String)", () -> s.lastIndexOf(nul));
        checkNpe("compareTo", () -> s.compareTo(nul));
        checkNpe("compareToIgnoreCase", () -> s.compareToIgnoreCase(nul));
        checkNpe("concat", () -> s.concat(nul));
        checkNpe("split", () -> s.split(nul));
        checkNpe("split(_,2)", () -> s.split(nul, OPAQUE_I[5]));
        checkNpe("matches", () -> s.matches(nul));
        checkNpe("replaceAll", () -> s.replaceAll(nul, "x"));
        checkNpe("transform", () -> s.transform(null));
        checkNpe("toUpperCase(Locale)", () -> s.toUpperCase((Locale) NULL_OBJ));
        checkNpe("toLowerCase(Locale)", () -> s.toLowerCase((Locale) NULL_OBJ));
        checkNpe("join(null delim)", () -> String.join(null, "a", "b"));
        checkNpe("join(null array)", () -> String.join(",", (CharSequence[]) NULL_OBJ));
        checkNpe("join(null iterable)", () -> String.join(",", (Iterable<CharSequence>) NULL_OBJ));
        checkNpe("copyValueOf", () -> String.copyValueOf((char[]) NULL_OBJ));
        checkNpe("valueOf(char[])", () -> String.valueOf((char[]) NULL_OBJ));
        checkNpe("new String(char[])", () -> new String((char[]) NULL_OBJ));
        checkNpe("getChars(null dst)", () -> {
            s.getChars(OPAQUE_I[3], OPAQUE_I[4], (char[]) NULL_OBJ, OPAQUE_I[3]);
            return null;
        });

        // getChars checks the SOURCE range before it looks at dst, so a bad
        // range plus a null dst is a StringIndexOutOfBoundsException — and the
        // destination-range failure is ALSO StringIndexOutOfBounds, not
        // ArrayIndexOutOfBounds.
        t = null;
        try {
            s.getChars(OPAQUE_I[3], OPAQUE_I[13], (char[]) NULL_OBJ, OPAQUE_I[3]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "getChars(0, 9, null, 0) must report the SOURCE range first, as"
                        + " StringIndexOutOfBoundsException; got " + nameOf(t));
        t = null;
        try {
            s.getChars(OPAQUE_I[3], OPAQUE_I[6], new char[OPAQUE_I[6]], OPAQUE_I[5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "getChars(0, 3, new char[3], 2) must throw StringIndexOutOfBoundsException — NOT"
                        + " ArrayIndexOutOfBounds, String does its own checking; got " + nameOf(t));

        sectionEnd("strnull", 31);
    }

    /** Opaque null, so javac cannot fold a null-argument call at compile time. */
    static final Object NULL_OBJ = null;

    /** Asserts that `body` throws exactly java.lang.NullPointerException. */
    static void checkNpe(String what, java.util.concurrent.Callable<Object> body) {
        Throwable t = null;
        try {
            body.call();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "String." + what + " with a null argument must throw NullPointerException, got "
                        + nameOf(t));
    }
```

and register it, replacing exactly:

```java
        "bounds", "strictExact", "divmod",
```

with:

```java
        "bounds", "strnull", "strictExact", "divmod",
```

and replacing exactly:

```java
        } else if ("bounds".equals(name)) {
            bounds();
```

with:

```java
        } else if ("bounds".equals(name)) {
            bounds();
        } else if ("strnull".equals(name)) {
            strnull();
```

**Verified by this lane**: the `OPAQUE_I` indices used above (`[2] == -1`,
`[3] == 0`, `[4] == 1`, `[5] == 2`, `[6] == 3`, `[13] == 9`) against the array
declaration at `RJdkIntrinsics2.java:140-142`; and `java.util.Locale` is
already imported (line 7).

**Left for the owning lane**: the `sectionEnd("strnull", 31)` count was counted
by hand from the block above and should be re-counted against the file's
convention before landing; and `checkNpe` takes a `java.util.concurrent.Callable`
because several bodies are `void` while the rest return values — if the file
already has a helper of this shape, prefer it.
