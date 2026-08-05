# L3 — Trace and fix the last unclassified layout rows

**Status: DONE 2026-08-05.** Both classes are gone from the census, A/B'd
against the pre-fix binary on JDK 25 (Azure Linux) with every other row
byte-identical, and both new probes are byte-identical to HotSpot 25 in both
modes. Evidence in *Outcome* below; branch
`fix/jdk-only-l3-scanner-membername-20260805`.

**Owns:** `native-builtins/src/phases_early.rs`, `native-builtins/src/lang_invoke.rs`
— **plus `native-io/src/lib.rs`, which is where the Scanner writer actually
was.** See *What the brief got wrong*; L5 owns `native-io` for the
`register_with_kind` migration, which does not touch these functions.
**Gated on:** nothing. Smallest lane — good first task.
**Conflicts:** L12 §4 may touch `lang_invoke.rs`. Land L3 first; it is a few lines.
**Effort:** S

## Goal

Three rows in item 2's table have no classification yet:

| class | slots | writes | real desc | n |
|---|---|---|---|---|
| `java/util/Scanner` | 3, 4 | `Int` | `L` | 2 |
| `java/lang/invoke/MemberName` | 4 | `Int` | `L` | 14 |

Everything else in the table has a named writer and one of four fix shapes.
These do not, only because nobody has run the tracer on them.

## Steps

1. **Trace, do not grep.** Two rounds of grepping failed to find the
   `ClassLoaders` and `URI` writers; `overlay-bt` named both instantly with
   file:line.

   ```sh
   CRATONVM_DBG=overlay,overlay-all,overlay-bt=Scanner \
     cratonvm --real-jdk --java-home $JDK -cp probes JdkOnlyCensusLoadProbe 2>&1 \
     | grep -A 12 'rust writer'
   ```

   Repeat with `overlay-bt=MemberName`. The frame you want is index 2–3; frames
   0–1 are the hunter and `set_field`.

2. **Classify against the taxonomy** before writing any code:
   * *kind 1* — our slots on a real layout → guard by a field **name** the real
     class declares;
   * *kind 2* — right field, index against the wrong class →
     `resolve_field_index_by_class_id`;
   * *kind 3* — VM-internal, no real field → side table (see L1);
   * *kind 4* — right field, wrong representation → convert (see the `Proxy`
     fix, where an `int` had to become a `Proxy$Type` enum reference).

   Getting this wrong is silent. `Proxy` looked like kind 2 and was kind 4;
   resolving its `type` field by name finds a real field that our `int` still
   must not be written into.

3. `Scanner`'s synthetic model is allocated with 3 fields
   (`phases_early.rs`), yet the census reports writes at slots 3 and 4 — so
   either the model grew or a different writer is involved. Resolve that
   discrepancy before fixing; it may be a second site.

4. `MemberName` is in `lang_invoke.rs`, next to the `VarHandle` fix. Read
   `vh_has_synthetic_layout` first — the predicate style is the one to copy, and
   its doc comment records why the field-count version was inert.

## Verification

A/B the census against the pre-fix binary on the same probe: the `Scanner` rows
go 2 → 0 and `MemberName` 14 → 0, with every other row byte-identical. Both
probes vs HotSpot in both modes. `cargo test --release -p cratonvm-native-builtins --lib`.

## Done when

Both classes are gone from the census and the record's table records which of
the four kinds each was.

---

# Outcome

## What the brief got wrong, and what step 3 was pointing at

Step 3 was right to be suspicious and wrong about the answer. The model did not
grow: there were **two** `java.util.Scanner` implementations in the tree, over
**different layouts**.

* `native-builtins/src/phases_early.rs` — a THREE-slot model (source, position,
  delimiter-as-`String`). This is the one the brief was written from, and it is
  **dead in every build configuration.** It is reachable only from
  `register_synthetic_overrides`, which `register_builtins` calls at
  `vm_init.rs:1426`; `register_io_natives` runs at 1428, into a
  last-write-wins map. All twelve of its triples are among the 34 that
  `native-io` registers.
* `native-io/src/lib.rs` — a FIVE-slot model (input, position, delimiter as a
  `Pattern`, radix, closed). This is the live one, and
  `native_scanner_init_string` is what `overlay-bt` named, at lines 3948 and
  3949, in one run:

  ```
  [OVERLAY] destructive native set_field: class=java/util/Scanner slot=3 value=Int(10) real_field_desc='L'
  [OVERLAY]   rust writer:
     … at /data/data/wt-l3mn-20260805/native-io/src/lib.rs:3948:9
  ```

`--dump-native-registry` on a real-JDK run settles it independently: all 35
live `java/util/Scanner` entries name `native-io/src/lib.rs` as their
registration site and none records having overwritten anything. The dead bundle
is deleted, so the next reader finds one model.

**This is the lane-brief-under-reports-its-own-defect failure mode again**, the
one L1 hit. A brief scoped from the census sees only the rows the census can
see, and the census cannot see a same-kind write. `javap -p --module java.base
java.util.Scanner` (Temurin 25.0.3) puts the five model slots here:

| model slot | our meaning | real field | visible to the census? |
|---:|---|---|---|
| 0 | input text (`String`) | `buf` `Ljava/nio/CharBuffer;` | **no** — reference over reference |
| 1 | position (`Int`) | `position` `I` | right field, by coincidence |
| 2 | delimiter (`Pattern`) | `matcher` `Ljava/util/regex/Matcher;` | **no** — reference over reference |
| 3 | radix (`Int`) | `delimPattern` `Ljava/util/regex/Pattern;` | yes, 1 hit/run |
| 4 | closed (`Int`) | `hasNextPattern` `Ljava/util/regex/Pattern;` | yes, 1 hit/run |

Two of five were reported. The fix covers all five.

## The classifications

| class | slots | kind | why |
|---|---|---|---|
| `java/util/Scanner` | 1, 2, 3, 4 | **2** — right field, index computed against the wrong class | `position`, `delimPattern`, `radix` and `closed` all exist on the real class with the same meaning and a compatible descriptor. They now resolve through `resolve_field_index_by_class_id` on the receiver, falling back to the model slot for a fabricated stub — the idiom L2 established for `Properties.loadFactor`. |
| `java/util/Scanner` | 0 | **3** — VM-internal, no real field | The input text has no counterpart: real `buf` is a `CharBuffer`, and a `String` written there is a wrong-type reference the census cannot see and real `Scanner` bytecode would `ClassCastException` on. It moves to an identity-hash-keyed side table, the shape `SR_STATE` already uses for `StringReader` in the same file. No `ObjectRef` is stored, so no collector path has to remap it, and `close()` drops the entry. |
| `java/lang/invoke/MemberName` | 4 | **3** — VM-internal, no real field | `vmindex` is `@Injected` in HotSpot: the class file declares no field for it. Index 4 there is `method`, a `ResolvedMethodName`. Unlike the `ClassLoaders` case, kind 3 here needs **no** side table — see below. |

## MemberName: the sentinel had never worked

Four natives wrote `Int(1)` into slot 4 as a "resolved" vmindex sentinel
(`native_mhn_resolve`, `native_mhn_init`, `alloc_resolved_member_name`,
`lookup_reveal_direct`). **That value has never reached the object.**
`set_field` coerces by the declared descriptor, and
`coerce_field_value_by_descriptor` maps an `Int` written to an `L` slot to
`Object(None)` — which is precisely the condition
`overlay_write_is_destructive` reports, so the census row is itself the proof.
Both readers (`native_mhn_object_field_offset`, `native_mhn_get_member_vm_info`)
therefore already fell to their `_ => 0` arm on a real layout, and a fabricated
`MemberName`'s slots are `Ljava/lang/Object;` too, so the same coercion applies
there.

So the fix needed no storage: write the sentinel only when the layout is ours
(`mn_has_synthetic_layout`, keyed on the real class's `method` field — the same
by-NAME shape as `vh_has_synthetic_layout`, for the same reason), and the
readers answer the same 0 they always answered. `probes/L3MemberNameProbe` is
byte-identical pre-fix and post-fix; the whole behavioural diff of that change
is that `method` is no longer nulled.

The comment this replaces claimed the non-zero sentinel was *critical* because
the JDK's `SplitConstantPool` throws `ConstantPoolException("Bad CP index: 0")`
on `entryByIndex(0)`. Whatever that was true of, it cannot have been this
write. **Another claim in a comment that no measurement supported.**

The remaining named slots (`clazz`, `name`, `type`, `flags`, `resolution`) were
written by `set_field_by_name` *plus* a raw-index write in some places and by
only one of the two in others; `set_field_by_name` is a silent no-op on a
fabricated layout, and a raw index is wrong on any layout that reorders. They
are now one `mn_set(name, model_slot)` accessor with a matching `mn_get`, so
reads and writes agree by construction rather than by coincidence.

## Measurements

Azure Linux, Temurin 25.0.3, pre-fix binary `cvm-l3-base` (dev at `ded183df8`)
vs post-fix `cvm-l3-fix2`, same host, same probes.

**Census A/B — both probes × both modes:**

| row | pre | post |
|---|---:|---:|
| `java/util/Scanner` slot 3 `Int` over `L` | 1 | **0** |
| `java/util/Scanner` slot 4 `Int` over `L` | 1 | **0** |
| `java/lang/invoke/MemberName` slot 4 `Int` over `L` | 7 | **0** |
| `java/util/HashMap` slot 1 (the benign row) | 1651 / 537 | 1651 / 537 |

No other row appears in either arm. `JdkOnlyBreadthProbe`'s transcript is
byte-identical pre/post and to HotSpot; `JdkOnlyCensusLoadProbe`'s differs only
in an ephemeral TCP port, pre/post and against HotSpot alike.

**Behavioural probes, new, both byte-identical to HotSpot 25 in `--real-jdk`
and `--jdk-only`:** `probes/L3ScannerLayoutProbe`, `probes/L3MemberNameProbe`.

The Scanner probe went **RED on the pre-fix binary**, which is the point of
writing it: a census row going 1 → 0 says only that a write stopped happening.
`useRadix(16)` was coerced away, so `radix()` answered 10 and `nextInt()` on
`"ff"` threw `InputMismatchException` and killed the run, in both modes. That
is the census row's live consequence in eight lines of Java.

`cargo test --release -p cratonvm-native-builtins --lib` and
`-p cratonvm-native-io --lib`: green apart from
`net_phase_e::tests::re3_get_by_address_uses_hotspot_ipv6_text_and_concrete_layout`,
which fails identically on an unmodified `dev` checkout.

## Three divergences fixed alongside, and three filed

Diffing a probe against the host JDK does not stop at the row you came for.
With the radix row fixed, the next lines of the diff were real, and are fixed:

* **`sc.delimiter().pattern()` answered `\s+`.** The real default is
  `Scanner.WHITESPACE_PATTERN`, whose pattern string is `\p{javaWhitespace}+` —
  a Java-only character class the `regex` crate cannot compile. `cached_regex`
  maps that spelling onto the whitespace regex explicitly, rather than leaning
  on `delimiter_regex`'s compile-failure fallback, which would have produced
  the right answer by accident.
* **`next()` left the position past the delimiter.** Invisible to a run of
  `next()` calls, wrong for everything that reads the position back: after
  `new Scanner("10 20 hello\nsecond").next()` the JDK sits at the end of
  `hello`, so `nextLine()` returns the empty remainder of that line and only
  the second `nextLine()` returns `second`. Ours returned `second` from the
  first and threw from the second — the shape the JDK's own
  `java/util/Scanner/NextIntNextLineTest` exists to catch. The unit test
  covering it asserted `new_pos > 5`, which froze the divergence.
* **`unreflectConstructor(...).type()` answered `(int)void`.** A constructor
  handle's type returns the class. `alloc_method_handle` already makes the
  mirror-image adjustment at the other end of the descriptor (prepending the
  receiver for an unbound virtual/special handle), and its comment listed
  CONSTRUCTOR among the kinds that keep the raw descriptor. `MH_DESC` is
  untouched, so dispatch and the `invokeExact` arity check are unchanged.

## Merged with L4, which landed the same day

L4 replaced the value-tag-only detector with a **shadow-layout diff**: it
compares `synthetic_stub_fields` — the model the natives were written against —
against the real class's declared fields by NAME, at define time. That finds
exactly the same-kind writes this record's table could not report, and it
independently reached L3's step-3 answer ("`Scanner`'s model is
`instance_fields(5)`, and slots 3/4 are the real `delimPattern` /
`hasNextPattern`").

**Re-measured on the merged tree, with L4's detector rather than the one this
lane's A/B used** (Temurin 25.0.3, Windows, `JdkOnlyCensusLoadProbe`,
`--real-jdk`): `java/lang/invoke/MemberName` does not appear at all, and the
only `java/util/Scanner` rows left are

```
[OVERLAY-LAYOUT] java/util/Scanner — model has 5 slot(s), 1 disagree with the loaded image
[OVERLAY] suspect native get_field [model-slot]: class=java/util/Scanner slot=1 …
          model=_f1:Ljava/lang/Object; real=position:I verdict=TYPE
[OVERLAY] suspect native set_field [model-slot]: … same slot, same verdict
```

— one read and one write, both at slot 1, which is `position`, the field they
are supposed to be at. The `verdict=TYPE` disagreement is between the image and
the *model*, not the image and the write: `instance_fields(5)` declares
`_f1:Ljava/lang/Object;` where the image declares `position:I`. Both probes are
still byte-identical to HotSpot 25 in both modes on the merged binary, and
`cargo test --release` for `cratonvm-native-builtins` (3267) and
`cratonvm-native-io` (380) is fully green.

It could not check `Scanner` any further than that, for the reason L4 names for
itself: **a model slot that is anonymous asserts nothing.** That follow-up is
now taken, and taking it changed the answer — see below.

Filed rather than fixed:

* **`close()` now means something, and nothing else honoured it.** The `closed`
  flag was write-only — no read anywhere in the tree — so a closed `Scanner`
  kept answering, where the JDK's `ensureOpen()` raises
  `IllegalStateException`. Reads now go through one funnel (`scan_input`) that
  raises it, which is both the JDK behaviour and what bounds the side table.
  `toString()` deliberately still works on a closed scanner, as it does on the
  JDK. **This changes `Compatible` mode**, from *silently wrong* to *matching
  HotSpot*, the same way L2's `try_set_jdk_map_field` fix did.
* ~~**`register_t2_3_completion_natives` is never called**~~ — **FIXED,
  2026-08-05; see *The two follow-ups* below.**
* **`Scanner.match()` is unimplemented on every path.** Our natives never
  populate the real `matcher` / `matchValid` state, so `match()` throws
  `IllegalStateException` after `next()`, `nextInt()` and `findInLine()` alike,
  where HotSpot 25 returns `ab`, `7` and `42`. Measured, not inferred. Closing
  it means running a real `Matcher` on every token path — a redesign of these
  natives, not a fix to one of them — and fabricating a `MatchResult` instead
  is the defect family this whole record is about. `probes/L3ScannerSearchProbe`
  deliberately does not call `match()`, and says why.
(A fourth, filed and then fixed once measured: **`useDelimiter(String)` and
`delimiter()` fabricated an uncompiled `java.util.regex.Pattern`** — two field
pokes, no `compile()`. The two slots they wrote are the real class's first two,
`pattern:String` and `flags:int`, so no census ever objected and our own
readers, which want the source string, were satisfied. It was still not a
usable `Pattern`: real `Pattern.matcher()` compiles lazily, gets as far as
running, and then throws, because everything a real `compile()` fills in was
left zeroed. Measured — `sc.useDelimiter(","); sc.delimiter().matcher("x,y")
.find()` answers `true` on HotSpot 25 and threw
`ArrayIndexOutOfBoundsException` inside `Matcher.search` here. Both sites now
ask the JDK for the object, keeping the fabricated one only as the fallback
for a runtime that cannot invoke `Pattern.compile`. `L3ScannerLayoutProbe`
covers it, and still matches HotSpot line for line in both modes. Filing it and
then measuring it is the order this record recommends: the first draft of the
note called it "correct for our readers and wrong for real JDK code", which was
a guess that happened to be right — the reproduction is what made it a
finding.)

---

# The two follow-ups, 2026-08-05

Both were taken after this record was first written. Both changed shape once
measured, which is the only reason they are worth writing up.

## `findWithinHorizon` — registered, not deleted

The record said the natives were dead and called it "a registration gap, not a
layout one". Two readings of that were open: wire them up, or delete them and
let real bytecode run — the direction the whole feature is going.
`probes/L3ScannerSearchProbe` decided it against Temurin 25.0.3:

```
fwh.s.found=123          HotSpot     fwh.s.found=java.lang.NullPointerException   CratonVM
fwh.b.short=null                     fwh.b.short=java.lang.NullPointerException
fwh.b.negative=IllegalArgumentException
```

Real bytecode does not cope: it reads `buf`, `matcher` and `source`, none of
which our natives populate, and NPEs in both modes. `findInLine` and `skip` —
the registered siblings in the same probe — matched HotSpot exactly, which is
what makes the three `fwh` lines a finding rather than a broken probe.

So they are registered, and the implementation moved from `phases_early.rs` to
`native-io` beside `findInLine` and `skip`: same state accessors, same regex
engine, and `register_scanner_natives` is live in every configuration. Moving
it rather than wiring it up in place is the same judgement as deleting the dead
Scanner bundle — a second implementation over a second model is what made this
lane's own brief describe `Scanner` as three fields wide.

One real defect came out of the move: the horizon is **code points**, and the
old implementation added it to a byte offset before walking back to a char
boundary. Identical for ASCII, wrong otherwise; there is now a test that fails
on the old arithmetic.

**The L6 ratchet caught it, and was right.** Two new `Bridge` rows shadowing
concrete bytecode is a regression it refuses — `bridge_shadows_bytecode` 4755
→ 4757, `bridge_without_acc_native` 10069 → 10071 on Linux — and its message
says the fix is to state the kind, not to raise the baseline. It is right:
`java.util.Scanner` declares no ACC_NATIVE method, contract §1.5 defines a
Bridge by an ACC_NATIVE target, and these two are a Rust fast path replicating
real bytecode, which is Intrinsic — the category the implementation carried
before it moved. Registered with `register_with_kind`, the Linux ratchet passes
on the **unchanged** baseline: 10069 and 4755 exactly. No re-freeze was needed,
and the first draft of this change would have re-frozen it for no reason.

## Naming the model — two slots, not five

The mechanical form of L4's follow-up was wrong, and measuring it is what
showed that. Naming all five slots in OUR order took the census from **one**
anonymous-model artifact to **three** index-wise disagreements:

```
[OVERLAY-LAYOUT] java/util/Scanner — model has 5 slot(s), 3 disagree with the loaded image
[OVERLAY] suspect native set_field [model-slot]: … slot=3
          model=radix:I real=delimPattern:Ljava/util/regex/Pattern; verdict=TYPE
```

None of those three is a defect. The shadow diff compares model slot *i*
against image slot *i*, and from slot 2 on the two orders diverge: the image
has `matcher`, `delimPattern`, `hasNextPattern` where the model means
delimiter, radix, closed, and does not reach `closed` and `radix` until slots
14 and 15. The natives resolve those three **by name** and never touch the
model index on a real layout, so the model claiming to correspond there is a
false statement that generates false rows.

Only `buf` and `position` line up, so only those are named. The rest stay
anonymous, which is the honest thing for a model that does not claim to match —
and the census now reports

```
[OVERLAY-LAYOUT] java/util/Scanner — model has 5 slot(s), 0 disagree with the loaded image
```

with no suspect rows at all, on Windows and on Azure Linux.

Naming `position` is not cosmetic. Every `_fN` is `Ljava/lang/Object;`, so on a
FABRICATED `Scanner` the `Int` position was coerced to null on the way in and
`scan_pos` read back 0 forever — `next()` would return the first token and
never advance. Declaring it `I` keeps the value. Same defect and same fix as
the `StringReader` position documented in `native-io/src/lib.rs`.

**The general lesson for the rest of item 2's work list**, since 73 of 156
modelled classes have disagreeing slots: *name a model slot only where the
model's index is meant to be the image's index.* For a class whose natives have
been migrated to by-name resolution, most slots no longer make that claim, and
naming them re-introduces exactly the wrong-field assertion the migration
removed.

## Verification

Three probes — `L3ScannerLayoutProbe`, `L3MemberNameProbe`,
`L3ScannerSearchProbe` — byte-identical to HotSpot 25 in `--real-jdk` and
`--jdk-only`, on Windows and on Azure Linux. Census: zero suspect rows for
`Scanner` and `MemberName`, `0 disagree` on the layout line. Bridge ratchet:
PASS on the unchanged linux baseline. `cargo test`: `cratonvm-classloading`
759, `cratonvm-native-io` 386, `cratonvm-native-builtins` 3270, all green.
