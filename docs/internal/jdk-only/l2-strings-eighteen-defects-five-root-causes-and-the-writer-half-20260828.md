# L2 — the `StringBuilder` family: eighteen defects, five root causes, 747 rows at 0 diffs

**Status: CLOSED.** 2026-08-28, branch `claude/l2-strings-20260828`, worktree
`/data/cvm-l2s-20260828` on the Linux build host. Oracle: HotSpot
`jdk-25.0.4+7` (`/data/jdkimages/jdk25-linux`), the same image CratonVM ran
against, so the two differ only in the VM.

Lane L2 of `HANDOFF-20260828-SCOPE`. It also closes `WORKER-3-NOTE-3`, which
that brief said to read first: nobody was on it, and its N1 residual — the
object-model migration — turned out to be the root cause of three of this lane's
defects.

Everything below was run. Nothing here is argued from a grep.

---

## 0. The result in one block

```text
probes/StringBuilderShadowSweep.java     747 rows
  HotSpot jdk-25.0.4+7                   747   (the oracle)
  cratonvm --java-home <same image>      747   0 differing lines
  cratonvm --jdk-only                    747   0 differing lines

native-won triples in the three families, before   118   all 118 probed
native-won triples after                           106   (StringBuffer's 58 became
                                                          AbstractStringBuilder's)
registry rows, real-JDK mode              10997 -> 10935  (-62)
java/lang/StringBuffer registrations          62 -> 0
cargo test -p cratonvm-native-builtins --lib   4177 passed, 0 failed
```

`owns_slot: true` for all 62 rows of each of the three classes, every one
registered by `native-builtins/src/lang_string.rs`, checked with
`--dump-native-registry` **before** any edit. There is no duplicate registrar for
this family and no half-fixed twin.

---

## 1. The eighteen defects

Every row is `HotSpot` on the left, `CratonVM before` on the right. All are
MEASURED, none inferred.

### Ten null contracts — the fabricated-success shape

| call | HotSpot | CratonVM |
| --- | --- | --- |
| `new StringBuilder((String) null)` | NPE | a usable builder, `length() == 0` |
| `new StringBuilder((CharSequence) null)` | NPE | a usable builder, `length() == 0` |
| `append((char[]) null)` | NPE | silent no-op |
| `append((char[]) null, off, len)` | NPE | silent no-op |
| `insert(1, (char[]) null)` | NPE | silent no-op |
| `insert(1, (char[]) null, 0, 1)` | NPE | silent no-op |
| `indexOf(null)` | NPE | `-1` |
| `indexOf(null, from)` | NPE | `-1` |
| `lastIndexOf(null)` | NPE | `-1` |
| `lastIndexOf(null, from)` | NPE | `-1` |

`-1` is the answer for "not present". A caller cannot tell it from "you passed
null", and a loop guarded by the exception never leaves.

Both `StringBuffer` constructors were the same defect through a different door,
and both are fixed by the retirement in §3 rather than by an edit: real
`StringBuffer(String)` is `this(str.length() + 16); append(str);`, so the NPE
comes from the JDK's own `str.length()`.

### Two orderings

`AbstractStringBuilder.insert(int, char[])` is `checkOffset(offset, count);
int len = str.length;` — destination first, dereference second — and the
four-argument overload is the same shape. Both natives read the array (and its
length) up front, so **both orders were wrong at once**: `insert(9, (char[])
null)` on a three-character builder is a `StringIndexOutOfBoundsException` on
HotSpot and was a silent no-op here, and `insert(1, (char[]) null)` is an NPE and
was the same silent no-op. The source read now happens after the check, and both
orders are pinned by probe rows.

### Three that are one root cause — the object model

`chars()`, `codePoints()`, `compareTo` and `writeObject` are real
`AbstractStringBuilder` bytecode that reads the `value` and `coder` **fields**,
not the `getValue()` / `getCoder()` accessors. A field read is something no
registered native can intercept, and every one of them read CratonVM's synthetic
`char[]` as though it were the JDK's compact `byte[]`:

```text
new StringBuilder("a€b").chars().toArray()
    HotSpot   [97, 8364, 98]
    CratonVM  [97,  172, 98]        0x20AC truncated to its low byte

new StringBuilder("😀").compareTo(new StringBuilder("z"))
    HotSpot   1                      0xD83D > 'z'
    CratonVM  -1                     0x3D  < 'z'

new ObjectOutputStream(..).writeObject(new StringBuilder("abc"))
    HotSpot   round-trips
    CratonVM  ArrayIndexOutOfBoundsException
```

**Every one of these passes on pure-ASCII content**, because a LATIN1 read of a
`char[]` whose units are all below 0x100 truncates to exactly the right bytes.
The first version of this probe asked `compareTo` only with ASCII and reported it
clean. §5 says what that cost.

### Two that are one root cause — `StringBuffer` is not `StringBuilder`

`register_string_builder_natives` bound one body for three classes, and
`StringBuffer`'s contract is not the other two's:

* **no monitor.** Two threads appending 4000 characters each to one
  `StringBuffer` finished with FEWER than 8000 characters, no exception
  anywhere. HotSpot answers 8000. A native that replaces a `synchronized` method
  and does not take the receiver's monitor loses updates silently.
* **no `toStringCache` invalidation.** `StringBuffer` caches its last
  `toString()`; a native that replaces the mutator never runs the line that nulls
  it. A stale `toString()` is the quietest failure shape in this whole surface —
  no exception, no wrong type, just an old answer.

### One ordering inside `insert`

`AbstractStringBuilder.insert(int, CharSequence, int, int)` shifts the tail right
and **then** reads `s.charAt(...)`, so every character is read from the array as
it stands after the shift and after the characters written before it. That is
unobservable for any other sequence and fully observable when `s == this`:

```text
new StringBuilder("abc").insert(1, b)            HotSpot aaaabc   was aabcbc
new StringBuilder("abcdef").insert(2, b, 1, 3)   HotSpot abbbcdef was abbccdef
```

It is a sequential dependency, not an artefact of leftover capacity: every read
is at an index below the pre-insert length, so the region the shift leaves stale
is written before it could ever be read. Two further probe rows —
`insert itself over discarded capacity` and `insert itself after a delete`, both
of which set up a capacity region full of discarded characters — confirm it:
HotSpot answers `aaab` for both, with no discarded character in the result.

---

## 2. `chars()` was not on the worklist, and that is the finding

`chars`, `codePoints` and `compareTo` have **no native registration at all**, so
they appear in no `native-shadows-bytecode` row and on no lane's list of triples.
The `--jdk-only` report can only name a native that exists. A registrar's
worklist is therefore an under-count of its family's surface by exactly the
methods nobody wrote a native for — which is also exactly the set that runs real
bytecode against a layout it does not have.

**Read the class's public API against the registrar's list, not only the
report.** For this family the gap was six methods (`chars`, `codePoints`,
`compareTo`, `isEmpty`, `offsetByCodePoints`, `subSequence`); three of them were
broken and three were fine, and no instrument in this campaign would have
distinguished them.

---

## 3. The retirement — 62 rows, and it removes two defects rather than costing any

MEASURED, `javap -p -c --system <jdk-25.0.4+7> java.lang.StringBuffer`, every
method classified: **every one is either a `synchronized` delegation to `super`
or a body that touches only its OWN `toStringCache` / `count` before
delegating.** Not one reads `value` or `coder` — the two fields the natives on
`AbstractStringBuilder` exist to serve. `writeObject` is the single exception,
and that is serialization, not dispatch.

So `java/lang/StringBuffer` is gone from `register_essential_natives_with_shims`.
Its own bytecode now runs and supplies the monitor and the cache invalidation,
and the layout work still happens natively one frame down:
`StringBuffer.append(String)` is `toStringCache = null; super.append(str);
return this;`, and that `invokespecial` lands on
`java/lang/AbstractStringBuilder.append`, which is still registered.

**`register_synthetic_overrides` keeps its own `StringBuffer` pair.** The
synthetic arm has no `StringBuffer` bytecode at all, so retiring there would
leave the class with no implementation; the split is real-JDK-only and stated at
the site.

**The paired edit that makes the retirement stick.**
`is_string_builder_layout_native_override` also drops `java/lang/StringBuffer`.
Left in, the force-native gate would resolve `StringBuffer.append` by walking to
the INHERITED `AbstractStringBuilder.append` native and running it directly —
skipping the `StringBuffer` body, and with it both the monitor and the cache. A
retirement silently undone by the gate that outlived it. A test asserts it
operation by operation rather than once, so a future widening of that list cannot
quietly re-take the class.

### What the retirement broke, and what that taught

Six rows went red the moment it landed:
`buf cache insert(int|long|float|double|boolean|CharSequence)` answered the
STALE `toString()`. Those six `StringBuffer` bodies are the only mutators that do
NOT null `toStringCache` themselves — because they do not have to:
`StringBuffer.insert(int, int)` is `invokespecial
AbstractStringBuilder.insert(II)`, and the JDK's `AbstractStringBuilder.insert`
then re-dispatches **virtually** to `this.insert(offset, String.valueOf(i))`,
landing back in the synchronized `StringBuffer` override that does null it.

**A native registered on an abstract base renders the value itself and never
re-enters, so the subclass's hook never fires.** That is the shared-surface
hazard the lane brief predicted, in its exact predicted form. The fix is one line
at the one chokepoint every mutation passes through (`sb_set_count`), guarded by
`count_slot + 1 < num_fields` — "this receiver has state after `count`", true of
`StringBuffer` on every image and false of `StringBuilder` on every image — so
the hot builder path pays one integer comparison and never a second name
resolution.

---

## 4. `WORKER-3-NOTE-3` N1 — the writer half, landed

That note's §3 named the residual exactly: CratonVM keeps two incompatible
representations of one class, `sb_value_units` taught the READER both, and every
writer still converted whatever it found into a `char[]` — **which is what
PRODUCED the torn object rather than tolerating it**.

`sb_store_units` is the writer half. The payload that is already there decides
the layout; a builder real bytecode constructed is written back compact, one
allocated under the synthetic 2-slot class is written back as a `char[]`, and
only a builder with no payload at all asks the class (the presence of a field
named `coder` is the whole test). Around it:

| helper | what it owns |
| --- | --- |
| `sb_layout` | which of the two representations this receiver holds |
| `sb_capacity_units` | `value.length >> coder`, the JDK's own expression |
| `sb_grow_units` | `newCapacity`: `max(2 * old + 2, needed)`, never shrinking |
| `sb_units_range` / `sb_unit_at` | reads, layout resolved once per call |
| `sb_store_units` | the whole payload, in the receiver's layout, coder included |
| `sb_append_units` | in place when it fits and the coder can hold it, else the above |
| `sb_put_unit` | one character in place, `false` when the array must inflate |
| `sb_alloc_payload` | a constructor's fresh payload, in the CLASS's layout |

Two consequences worth stating because they are contract, not implementation:

* **the JDK inflates and never deflates.** Once a builder has held a non-LATIN1
  character its `coder` stays UTF16 for the life of the value array, and
  `capacity()` is `value.length >> coder`. Deflating on the way back down would
  DOUBLE the reported capacity; the probe row `delete back to all-latin1 keeps
  capacity` asserts it does not.
* **`sb_set_count` no longer writes `coder`.** It used to force LATIN1 on every
  count update, on the premise that "the payload these natives maintain is a
  `char[]`". Since the migration that premise is false, and a blind zero there
  would tell every real-bytecode reader that a UTF16 payload was LATIN1 — the
  same wrong answer by a shorter route.

`getValue()` now hands back the real array for a compact receiver and `getCoder()`
the real field. They must agree, and deriving the coder from the CONTENT (what
`getCoder` did) breaks that agreement for an inflated builder whose current
content happens to be all LATIN1 — `String.nonSyncContentEquals` compares the two
arrays only when the coders match, so a disagreement is a wrong answer, not a
slow one.

---

## 5. What PASSED — this is where the work is NOT

A record that lists only failures tells the next person nothing about coverage.
All of the following were asked and were already right, in both modes:

* **every bounds and refusal edge of the mutators.** `delete` clamping `end`
  down but rejecting a `start` past the length; `deleteCharAt(length())`;
  `setLength(-1)` as `StringIndexOutOfBoundsException` while `new
  StringBuilder(-1)` is `NegativeArraySizeException`, which are deliberately
  different classes; `setCharAt(length())`; `replace` with a reversed range;
  `substring(len)` valid and `substring(len + 1)` not; the whole `codePoint*`
  family including a window that splits a surrogate pair.
* **`off + len` overflowing to a negative int** on `append(char[], int, int)` and
  `insert(int, char[], int, int)` — the case a check written `off + len >
  b.length` gets wrong while looking right. The i64 widening was already there.
* **`append(null)` for the three overloads that differ.** `String` and
  `CharSequence` append the text "null"; `char[]` throws. And
  `append((CharSequence) null, 1, 3)` applies the window to the four characters
  of the literal — it appends "ul" — rather than throwing.
* **`reverse()` over surrogates**, both directions: a valid pair is preserved,
  and `"\uDC00\uD800"` reverses INTO a valid pair, which is what the javadoc
  promises and a plain code-unit reversal does not do.
* **`appendCodePoint`** for a supplementary code point, for a lone surrogate
  (which is a valid code point and is appended), and its `IllegalArgumentException`
  for `-1`, `0x110000` and `Integer.MAX_VALUE`.
* **`capacity()` growth**: `new StringBuilder()` is 16, `new StringBuilder(s)` is
  `s.length() + 16`, `ensureCapacity` follows `max(2 * old + 2, requested)`,
  `trimToSize` lands exactly on the length, and an inflating append leaves the
  capacity unchanged.
* **the `repeat` family** (JDK 21+), including a negative count, a null sequence
  with a negative count, and a supplementary code point.
* **every reader on non-LATIN1 content through a native door** — `charAt`,
  `substring`, `indexOf`, `lastIndexOf`, `getChars`, `subSequence`,
  `codePointCount`, `String.valueOf`, `contentEquals`, string concatenation and
  `String.format`. The natives themselves have always handled UTF-16 correctly;
  it was only the real-bytecode readers of the raw fields that did not.
* **the interface doors** — `Appendable.append` in all three arities and
  `CharSequence.length` / `charAt` / `isEmpty` / `subSequence` / `chars` on both
  concrete classes.
* **a `CharSequence` whose `toString()` LIES.** Every sequence-taking overload was
  asked with a sequence whose `charAt` and `toString` disagree, which separates a
  shim that reads the sequence the way `AbstractStringBuilder` does (by `charAt`)
  from one that calls `toString()`. All correct.

**And the one that matters most for method:** the first version of this probe
asked `compareTo` with ASCII only and reported it clean. It is one of the three
LATIN1-truncation defects. The row that caught it is the one that compares a
surrogate pair against `'z'`; the rows that compare `€` against `₭` and
against `'b'` pass on the broken build, because truncation preserves the SIGN
often enough to look right. A probe that asks a comparison only for its sign, or
only on ASCII, will report this family clean when it is not.

---

## 6. What this does NOT establish

* **No performance measurement was taken.** `StringBuilder.append` is among the
  hottest paths in this VM and the write path now resolves a layout per call
  (two field reads and, for a compact receiver, a name resolution that was
  already being paid by `sb_set_count`). The in-place append arm allocates
  nothing and reads no whole payload, and the growth rule is unchanged, so the
  asymptotics are the same — but that is an argument, not a number. **Nobody has
  priced this.** See §7 N1.
* **`StringBuffer` gained a Java frame per call** by the retirement. Same
  caveat, smaller surface.
* **Mockito and Byte Buddy were not run.** `WORKER-3-NOTE-3` argued the
  production reachability of the torn object from source comments and from the
  cglib record the tree already holds; this lane reproduces the defects through a
  different door — real `AbstractStringBuilder` bytecode reading its own fields —
  and does not re-derive that argument. What it does establish is that the tear
  is gone at the source: a builder is never converted from one representation to
  the other.
* **The `--synthetic-jdk` arm is unchanged by construction, not by measurement of
  that arm.** `register_synthetic_overrides` keeps its `StringBuffer` pair,
  `sb_layout` answers `Synthetic` for a two-field receiver, and the 4177
  `native-builtins` lib tests — which run against a mock context with no class
  model, i.e. exactly the synthetic shape — all pass. No synthetic-jdk corpus arm
  was run.
* **The probe's serialization rows request a compatibility class.**
  `ObjectInputFilter$Config.<clinit>` calls `System.getLogger` unconditionally,
  and under `--jdk-only` the fabrication is REFUSED and
  `craton_alloc_system_logger` falls back to the real class — which is the
  behaviour that site already documents. The report records the REQUEST;
  `counts.compatibility_classes` is **0**, so nothing fabricated was
  instantiated, and the ten serialization rows round-trip correctly. Named here
  because a DoD screen counting requests will see it.

---

## 7. What is left, and where it lives

Three items, none of them a correctness question, are carried on the OPEN page
`l2-strings-residuals-the-migration-is-unpriced` under
`docs/known-issues/jdk-only/` rather than here, because a reader looking for open
work does not read the retired records:

* **N1 — the write path is unpriced.** `StringBuilder.append` is among the
  hottest paths in this VM and this change is on it. `f52fa3fa6` — this branch's
  own previous commit, which has the null fixes and the retirement but NOT the
  migration — is the exact control.
* **N2 — `java/lang/StringBuilder`'s own 62 rows are a candidate retirement,
  DECLINED**, for two stated reasons rather than guessed at.
* **N3 — `chars`, `codePoints` and `compareTo` are correct because the layout
  is**, not because anyone registered them, and the probe rows are the only
  guard.

`WORKER-3-NOTE-3`'s OTHER nominations are untouched by this lane and stay open on
that page: its N2 (the `H25-1` §1.4 dead list needs the R2 correction), its N3
(the `MethodHandle.invoke` losers in `lib.rs`) and its §7 refusal of the
`java.lang.invoke` block as a CAPABILITY gap all stand exactly as written. **This
lane closes its N1 only**, and that note's banner says so.

---

## INDEX ROW

* `l2-strings-...` — L2's three families: 118 native-won triples probed at 747
  rows, 0 diffs in both modes. Eighteen defects with five root causes — ten null
  contracts, two dereference orderings, the object-model tear (`chars`,
  `codePoints`, `compareTo`, `writeObject` all read `value`/`coder` as LATIN1 and
  all pass on ASCII), `StringBuffer`'s missing monitor and stale
  `toStringCache`, and `insert`'s read-after-shift. 62 `StringBuffer` shadows
  retired to the class's own synchronized bodies. **Closes `WORKER-3-NOTE-3`
  N1.** CLOSED.
