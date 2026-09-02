# G63-1 — sweeping the hazard instead of the row, and a refactor I started and refused

> **ID COLLISION — there are TWO records numbered `G63-1`, written the same day
> by two lanes that could not see each other.** This one is the surrogate sweep.
> The other is
> `G63-1-the-values-view-iterator-is-not-fail-fast-20260817.md`, on
> `map.values().iterator()`.
>
> **A citation of "G63-1" is therefore ambiguous, and it has already misled
> once**: "G63-1's own N1, the one-line strict-mode fix" means the OTHER
> record's N1. This record's N1 is the 27-site `native-collections` units
> refactor, which is not one line and is not strict-mode-only. Cite by title,
> not by number, until one of the two is renumbered — neither was renamed here
> because both are already cited by number in landed commit messages, where a
> rename cannot follow.


**Status:** MEASURED throughout, fixes included (see the banner). **Provenance:**
every row is MEASURED on both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-rel6` and `target-rel7` under `--jdk-only`. Probes:
`scratchpad/g63/{SurrSweep,Decomp,Sweep2}.java`, ASCII labels only, every row
printed as CHAR VALUES rather than text so the console code page cannot affect
the comparison.

> **MEASURED 2026-08-17 at `048dcb799`.** Both fixes are in a binary and the
> arm is re-run: `RJdkBridge1` **170 checks, empty diff**, and the full
> `--jdk-only` suite **99 of 99, 0 failed** with all seven new assertions
> live.
>
> **`String.join` passed with no fix of its own**, which is what the row was
> written to find out: it is unregistered, so it inherits `String.valueOf`,
> and §2's "if it does not, the row will say so" resolved in favour of the
> `valueOf` change carrying it. `StringJoiner` needs no separate work and N3
> below is closed by measurement rather than by argument.

---

## 0. Why sweep at all

The suite is 99 of 99. With no red row to follow, a new defect has to be
*looked for*. `RJdkBridge1`'s `surrog` family already names the hazard —
"a Rust `str` cannot hold an unpaired UTF-16 surrogate, so any bridge that
round-trips a Java String through one either rejects it, replaces it with
U+FFFD, or reshapes it" — and names eight classes. **The hazard is systematic;
eight classes is a sample, not a population.**

Two sweeps, 54 rows across surfaces no vector touches. **48 matched.** Six did
not.

## 1. Round one — 33 rows, three divergences, one of them mine

```text
  replace_cs   HotSpot 7a,d800,62   CratonVM 7a,fffd,62
  join         HotSpot 61,d800,62   CratonVM 61,fffd,62
  split_join   HotSpot 61,d800      CratonVM 61,fffd
```

Decomposing the three was the step that mattered:

* `split` is **innocent**. My probe composed `split` with `join`, and the
  `join` lost the unit. A one-row reading would have filed a defect against
  the wrong method — the composed probe is a convenience that costs
  attribution, and the decomposition is what recovered it.
* `String.valueOf((Object) s)` loses it — and `StringJoiner.toString()` does
  too, which is where `String.join` (unregistered, real bytecode) inherits it.
* `String.replace(CharSequence, CharSequence)` loses it **on a call that
  matches nothing**. That row is the discriminating one: the loss is the round
  trip, not the replacement.

## 2. The two fixes

**`String.valueOf(Object)` is `obj.toString()`**, so for a String it is an
IDENTITY — `String.valueOf(s) == s` on HotSpot, asserted as a vector row. Ours
decoded to Rust text and built a second String on **every call** of a very hot
method: wrong, and an allocation. It now returns the object. The non-String arm
moves to `invoke_to_string_units_opt`, which `G26` built for exactly this
reason and which this call site never adopted — the lossy wrapper over it
exists only to produce a Rust `String`.

**`replace(CharSequence, CharSequence)` now works in units end to end.**
`replace_units` is the *only* implementation rather than a second one behind a
surrogate guard, because `sb_string_from_units` already takes the plain-text
path when the units are representable — so every input that works today is
unchanged and there is no rarely-taken copy to drift. The empty-target case is
handled explicitly: both Java and Rust give `-a-b-c-`, and the obvious loop
gives `a-b-c`.

Five vector rows added to `surrog` (24 → 29), all verified PASS on HotSpot
before the fix was built. `String.join` gets a row and **no fix**: it should
follow from `valueOf`, and if it does not, the row will say so rather than my
guessing.

## 3. Round two — 21 rows, four divergences, all one shape

```text
  list_toString    HotSpot 5b,61,d800,62,5d      CratonVM 5b,61,fffd,62,5d
  map_toString     HotSpot 7b,6b,3d,61,d800,...  CratonVM 7b,6b,3d,61,fffd,...
  arrays_toString  HotSpot 5b,61,d800,62,5d      CratonVM 5b,61,fffd,62,5d
  cbuf_wrap        HotSpot 61,d800,62            CratonVM 61,fffd,62
```

Seventeen rows in that sweep are exact, including `String.format`,
`MessageFormat`, four `StringBuilder` mutators, `Properties.store`/`load`
round-trip, UTF-16 encode/decode, and `Writer.write`.

The first three are `G55-1` N3, and they are **one defect in seventeen
places**: every collection `toString` in `native-collections` bottoms out in
`obj_to_display_string`, which returns a Rust `String`.

## 4. The refactor I started and refused

`G55-1` N3 says `native-collections` "has no units-preserving String reader,
and cannot get one" — it does not depend on `native-builtins`, and a local
decoder would be a second encoding of the compact-string layout. It nominates
a fourth `NativeContext` method beside the three units-exact ones that already
exist, owner `native-api`.

**I built it, and then reverted it.** The mechanism half is genuinely small,
and this record exists so the next person does not re-derive it:

* `vm/src/vm/vm_object.rs:788` already has
  `pub fn read_java_string_units(heap, obj) -> Option<Vec<u16>>` — the exact
  reader exists and is already used by `invokedynamic.rs`.
* The trait method is ~15 lines: a lossy default (`read_string` +
  `encode_utf16`) so no implementation regresses, and a VM override that calls
  the above, guarded by `is_real_java_string` for the same reason
  `java_strings_equal` is — the structural reader duck-types a String from
  field 0, and a CratonVM synthetic `StringBuilder` is also `char[]`-backed
  with an over-allocated buffer.

**What stops it is the consumer, and the consumer is 27 call sites.**
`obj_to_display_string` returns `Result<String, _>`; its callers `push_str`
and `format!` into Rust `String`s and there are seventeen `native_*_to_string`
functions above them. Converting three of seventeen would fix the three rows I
happened to measure and leave fourteen siblings with the identical defect —
which is this directory's own "do not generalise a contract from three rows"
read backwards, and worse than not starting.

So the trait method was reverted rather than landed unreached. **That is the
specific failure this session criticised twice** — `record_local_cert_chain`
existed only in a doc comment (`G51-1` §3c), and the whole `BaisEvent`
mechanism was built, tested and dispatched with no consumer for a week
(`G58-1` §1). Adding a fourth would have been worse for knowing better.

It is also the same standard applied to `G61-1` N2 an hour earlier, where
converting `url_parse` to units was refused for the same reason: wide blast
radius, narrow evidence.

## 5. NOMINATIONS

**N1 — DONE 2026-08-18 at `1410c0a4b`**, in
`G70-1-the-refactor-i-refused-twice-and-then-did-20260818.md`. Two
corrections to what is written below: it needed **two** trait methods, not
one — a value read losslessly and written back through `create_string` is
lossy again at the last step, and `init_string_from_units` cannot stand in
because it assumes the `char[]` layout a real JDK String does not use. And
the population was **21 of 29 probe rows**, not the three §3 measured. Kept
in full below because the reasoning for refusing it at the time was right.

**N1 (original text) — `G55-1` N3, restated with its real cost and its real shape.** The
`native-api` half is ~15 lines and §4 gives it in full. The work is the
`native-collections` half: make `obj_to_display_string` return `Vec<u16>`, add
a lossy adapter for the callers that genuinely want text, and take all
seventeen `native_*_to_string` families in ONE change with the collection
`toString` vectors as the net. It is mechanical, it is wide, and it wants a
pass of its own — not a corner of someone else's.

**N2 — `CharBuffer.wrap(char[]).toString()`.** Measured in §3 and NOT part of
N1: `java.nio.CharBuffer` is not `native-collections`. Unowned and
uninvestigated.

**N3 — CLOSED by measurement.** `String.join` had a vector row and no fix, on
the prediction that it would inherit `String.valueOf`. It did: green at
`048dcb799`. Kept here rather than deleted, because a nomination that resolved
without work is worth as much to the next reader as one that needed it.

**N4 — the sweep is not finished.** 54 rows is two afternoons of surfaces, not
a corpus. Untouched and known to be reachable: `Base64`, `URLEncoder`/`Decoder`,
`Collator`, `Normalizer`, `String.chars()`/`codePoints()` round trips,
`Scanner`, `MessageDigest` over text, and every `java.time` formatter. The
method costs about ten minutes per sweep and has now returned six defects for
two.
