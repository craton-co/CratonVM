# G70-1 — the refactor I refused twice, and the last mile nobody predicted

**Status:** MEASURED throughout; fixed, whole. **Provenance:** every row on
both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM `C:/craton/target-rel13`
before and after, `--jdk-only`. Probes `scratchpad/g72/{U,V}.java`, every row
printed as UTF-16 **unit values** so the console code page cannot affect the
comparison.

---

## 0. What was refused, and why it was right to refuse it then

`G63-1` §4 built the `NativeContext` half of this, measured that it worked,
and **reverted it** — because the consumer was 27 call sites under 17
`native_*_to_string` families, and converting the three that had been measured
would have left fourteen siblings wrong in exactly the same way while looking
like the question had been settled. `G55-1` N3 had nominated it; `G67-1` §4 and
`G61-1` N2 refused two other wide changes on the same grounds in the same
session.

This is that pass. The refusals were right — and so was doing it eventually,
because the defect never got smaller:

```text
21 of 29 probe rows diverged before this change.
```

`G63-1` had sampled three (`list_toString`, `map_toString`, `arrays_toString`).
The population was 21.

## 1. The mechanism, and where the prediction was wrong

`G63-1` §4 predicted **one** new trait method, a units-exact reader, and
costed it at ~15 lines. The reader was right and the count was not: it needs
**two**, and the second is the interesting one.

* `read_string_units` — the units-exact counterpart of `read_string`, and the
  fourth member of the units-exact family beside `read_char_array_into`,
  `write_char_array_from` and `init_string_from_units`.
* `create_string_from_units` — **a value read losslessly and then written back
  through `create_string` is lossy again at the last step.** A reader alone
  fixes nothing.

`init_string_from_units` could not stand in for the second, which is why the
count was wrong rather than merely low: it fills an ALREADY-ALLOCATED receiver
and its default assumes the `char[]` layout. A real JDK `String` is `byte[]`
plus a coder, so that default is wrong in precisely the mode this matters most
in.

Both VM primitives already existed — `read_java_string_units` (`vm_object.rs`,
which `G63-1` found) and `create_java_string_from_units` (which it did not).
Both trait defaults are deliberately LOSSY, so no implementation regresses by
not overriding, and the hazard stays exactly where it already was for anyone
who does not.

## 2. Two defects found by converting, neither of them predicted

**A boxed `Character` can itself be a lone surrogate.**
`Character.valueOf('\ud800')` is legal, and `char::from_u32` rejects exactly
the surrogate range, so `unwrap_or('?')` printed **`?`** — not even U+FFFD,
which at least says "something was here". Present twice, in
`native-collections` and in `invoke_to_string_units_opt`. A units renderer
does not need the `char` type at all; the fix is `vec![v as u16]`.

**`StringBuilder.append(Object)` was still calling the lossy reader.** This is
the one worth carrying:

```text
                       HotSpot          CratonVM (after the collections fix)
sb.append((Object) s)  61 d800 62       61 fffd 62
sb.append(s)           61 d800 62       61 d800 62      <- exact, right beside it
```

`invoke_to_string_units` is `invoke_to_string`'s exact twin — same `"null"`
fallback, same signature shape — it was built by `G26` for this hazard, and it
sits **directly above the caller in the same file**, unused by it. That is the
`G51-1`/`G58-1` shape (a mechanism with no consumer) with one difference that
makes it worse: the consumer existed and kept calling the lossy one.

## 3. The last mile, and why two rows survived

After converting all seventeen families, **two of 29 rows still failed** —
`Arrays.asList(s).toString()` and a nested `List<List<String>>`.

Both are collections. Neither is rendered by `native-collections`: a real JDK
`AbstractCollection.toString` is a `sb.append(e)` loop over `Object`, so both
came back through `StringBuilder.append(Object)` and lost the unit that
`native-collections` had just preserved.

**A whole-family fix that stops at the crate boundary is not whole.** The two
survivors are what turned §2's second defect from a guess into a measurement,
and the decomposition that found it (`V.java`: `append(String)` versus
`append(Object)` versus `String.valueOf` versus concat, on the same input) is
the same technique `G63-1` §1 used to clear `split` of a defect that belonged
to `join`.

## 4. One change that is not a rendering

The stream `sorted()` fallback builds a **sort key** from each element's
rendering. It moves to units too, and that one is not cosmetic: Java compares
Strings by UTF-16 code unit (`String.compareTo`), `Vec<u16>` compares
lexicographically by unit, so the units key is the closer match as well as the
lossless one. Two elements whose renderings differed only in an unpaired
surrogate used to collapse to the same U+FFFD key and sort arbitrarily.

## 5. What is guarded

22 rows in `RJdkBridge1`'s `surrog` family (34 → 56), **all verified PASS on
HotSpot before the fix was built**, covering all seventeen families by name
plus:

* a **well-formed surrogate pair**, which must survive as two units and not be
  reshaped by a renderer that now thinks in units;
* boxed numbers, so the Java-spec `Double`/`Float` spelling (`2.5`, the
  trailing `.0`, `Infinity`) is not lost to the rewrite;
* ordinary text and `Optional.empty`, which have no surrogate to carry.

The rows assert a POSITION, not merely the absence of U+FFFD, except for the
one row where the position is not knowable (a `keySet()` view of a map whose
key is ordinary text) — and that row says so at the helper rather than
silently weakening.

`native-collections` unit tests: 133 passed, 0 failed. Arms at `1410c0a4b`:
`--jdk-only` **100 of 100**; `SUITE=all` 95 of 100 and `SUITE=core` 61 of 62,
both at baseline with failure sets identical **by name**.

## 6. NOMINATIONS

**N1 — DONE (`G78-1`). `read_string` has ~2,900 other callers and most of them are fine.**
The two new trait methods make the units path available everywhere, which is
not the same as everywhere being converted. `read_string` remains correct
wherever the result is only INSPECTED — a class name, a charset name, a flag,
a descriptor — and wrong wherever it is handed back to Java. Nobody has
separated the two, and the grep alone will not: the distinction is what the
value is used FOR. `G63-1` N4's remaining surfaces (`Base64`, `URLEncoder`,
`Collator`, `MessageDigest` over text, every `java.time` formatter) are the
places to start, because they are known to be reachable.

> **Closed by `G78-1` on 2026-08-18.** The nomination was right that no grep
> makes the judgement, and right that the surfaces above were the place to
> start — but they were not where the defect was. Two filters cut 2904 hits to
> **18** without making any judgement at all: keep only blocks that both read
> and CREATE a string (dataflow), then keep only registrations the registry
> dump shows as live and owning under `--jdk-only` (reachability). Of the
> eighteen, `Base64`/`URLEncoder`/`Collator`/`MessageDigest`/`java.time` were
> all exact; `java.io.File` was the whole defect, losing the unit three times
> over and MERGING two distinct paths in `equals`/`hashCode`/`compareTo`.

**N2 — `StringBuffer` and `AbstractStringBuilder` share the fixed handler,
but the other `append` overloads were not swept.** `append(CharSequence)`,
`append(char[])` and `insert(...)` each have their own body, and only the ones
this probe happened to touch were measured. §2 is evidence that a sibling
sitting next to a fixed method is not thereby fixed.

**N3 — the `?` from `unwrap_or` is a shape, not two incidents.** Both
occurrences turned an unrepresentable unit into a character that means
something else, and neither was reachable from any vector before today. A grep
for `from_u32(...).unwrap_or` is cheap and nobody has run it against the whole
tree.
