# L4 — Close the overlay detector's three blind spots

**Status: DONE 2026-08-05.** All three gaps report. The census went from **4
distinct sites to 135**, and the widened detector found **23 slots across 12
classes where CratonVM's model names a different field than the image declares
at that index** — the reference-into-reference family (kind 5) that no previous
census could see by construction, 10 of them accessed on three small probes.
Measurements, the A/B against the pre-fix binary, and what the detector *still*
cannot see are in [§Outcome](#outcome-2026-08-05) below.

**Owns:** `vm/src/vm/vm_exec.rs`, the hunter only (~lines 3070–3200 and the
`NativeContextImpl::set_field` call site ~9150); plus
`classloading/src/shadow_layout.rs` (new) and the two class-define sites that
call it.
**Gated on:** nothing.
**Conflicts:** L11 also edits `vm_exec.rs`, in the dispatch regions (~14700,
~22700). Disjoint — but coordinate, and never `git add -A` blind.
**Effort:** M
**Evidence:** [`fabricated-object-layouts-leak-into-native-code.md`](../../known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md)

## Goal

`CRATONVM_DBG=overlay,overlay-all` produced item 2's entire work list — 24
slots across 13 classes, from three small probes. It is the most productive
instrument this feature has. It also **misses three whole categories**, so its
output is a floor and anyone treating it as a census will declare victory early.

## The three gaps

1. **Reads are uninstrumented.** The hunter sits on
   `NativeContextImpl::set_field`. A native that *reads* slot 3 of a real
   `Properties` gets a reference where it expects an int and silently
   misbehaves; nothing reports it. Reads are arguably the more common half.
2. **Same-kind wrong-slot writes are invisible.** The trigger is
   `overlay_write_is_destructive`, a *type* mismatch. Writing an `Int` into the
   wrong `Int` slot passes silently — and that is precisely the defect this
   item's title describes ("the index still resolves and points at a different
   field").
3. **`Object(None)` over a primitive is ignored.** `overlay_write_is_destructive`
   only flags `Object(Some(_))`. `native_props_init` writes a null to
   `PROPS_FIELD_DEFAULTS`, which is `loadFactor` on a real layout, and the
   hunter says nothing.

## Design

Gap 3 is a one-line predicate change and should land first — cheap, and it will
immediately widen the census.

Gap 1: mirror the hunter onto the read path. Same cold-log shape, same
`overlay-bt` support. Expect volume; the `overlay-all` suppression pattern is
the precedent for keeping it usable.

Gap 2 is the hard one and needs a different signal, because there is no type
mismatch to detect. Options, in increasing cost:

* **Name-check at the write site.** When the receiver's class is `BootImage`
  origin and the native writes by raw index, compare the declared field *name*
  at that index against what the caller believes it is writing. Requires the
  caller to state a name — i.e. an instrumented `set_field_named_slot(idx, name)`
  used at converted sites. Only covers migrated code, but it turns each
  conversion into a permanent assertion.
* **Shadow-layout diff.** For each class we fabricate, record our intended slot
  meanings; when the real class is loaded, diff the two layouts once and report
  every disagreeing index. Catches everything, including reads, without
  per-access cost. This is the strongest option and is probably the right one.

## Steps

1. Flag `Object(None)` over a primitive descriptor. Re-run the census; expect new
   rows (at minimum `Properties` slot 3).
2. Add the read-path hunter behind the same tokens.
3. Build the shadow-layout diff: at `ensure_synthetic_class` / fabrication time
   we already know the slot count we wanted; when the real class exists, emit one
   report per class rather than per access.
4. Re-run and update item 2's table. **The number will go up.** That is the
   point — an instrument that finds nothing new after being widened was not
   widened.

## Verification

**Inject a violation and watch it fire**, for each gap, then revert:

* write `Object(None)` to a known primitive slot → must report;
* read a known mismatched slot → must report;
* write an `Int` to a wrong `Int` slot → must report (gap 2 only).

A detector that reports nothing on injected input is decoration. Two guards
shipped on 2026-08-03 that were vacuous for exactly this reason.

## Done when

All three gaps report on injected input, item 2's table is re-measured with the
widened detector, and the record says explicitly what the detector still cannot
see.

---

## Outcome, 2026-08-05

### What landed

* **Gap 3** — `overlay_write_is_destructive` became `overlay_access_is_cross_type`
  and flags `Object(_)`, not `Object(Some(_))`, over a primitive descriptor. The
  old name was the reason for the old behaviour: a null coerces to a typed zero,
  so nothing is *destroyed* — but the native still believed it was clearing a
  reference field and still wrote a slot the image declares `int`. The predicate
  is named for the mismatch now, because that is what it tests.
* **Gap 1** — the hunter also sits on `NativeContextImpl::get_field`. It checks
  the **raw** slot contents, not the value the native receives: `get_field_as`
  has already coerced the storage tag to the declared descriptor, so by the time
  the caller sees it the disagreement is gone.
* **Gap 2** — the shadow-layout diff, `classloading/src/shadow_layout.rs`. The
  design listed two options and called this one "probably the right one"; it is.
  `synthetic_stub_fields` **is** the model the natives were written against —
  it sizes a bytecode `new` of the stub and it is what
  `define_class_with_options` pads a real class up to. So when a class has both
  a model and real bytes, the two layouts are simply diffed, once, at define
  time (both real-bytes paths: `define_class_with_options` and
  `upgrade_synthetic_class`). Every disagreeing index is reported once per
  class, and the access-site hunter then fires on those slots *whatever the
  value's tag* — which is the only way an `Int` written into the wrong `Int`
  slot is ever reported.

The name-check-at-the-write-site option was **not** taken. It only covers
migrated code, and with no migrated call sites it would have been an assertion
that cannot fail — the failure mode this feature has hit three times already.

### Measured — 3 probes × 2 modes, JDK 25 (Temurin 25.0.3), Windows

`JdkOnlyCensusLoadProbe`, `JdkOnlyBreadthProbe`, `MapLayoutMatrixProbe` under
`--real-jdk` and `--jdk-only`, `CRATONVM_DBG=overlay,overlay-all`, A/B against a
pre-fix binary built from the same tree at `ded183df8`.

| | pre | post |
|---|---:|---:|
| distinct `(op, class, slot, value kind, real desc)` sites | **4** | **135** |
| of which reads | 0 | **70** |
| modelled classes reached, defined from real bytes | — | 156 |
| of those, disagreeing with the image at ≥1 slot | — | **73** |
| disagreeing slots | — | **152** (129 `TYPE`, 23 `NAME`) |
| `NAME` slots — model names a different field | — | **23** across **12** classes |
| of those, accessed on these probes | — | **10** |

Reports by reason, summed over the six runs: `set_field [cross-type]` 8,370 ·
`get_field [cross-type]` 1,869 · `get_field [cross-type+model-slot]` 402 ·
`set_field [model-slot]` 238 · `get_field [model-slot]` 149 ·
`set_field [cross-type+model-slot]` 2.

**Every pre-fix row survives with its count**, which is what makes the two
numbers comparable rather than merely both large:

| row | pre | post |
|---|---:|---:|
| `java/util/HashMap` slot 1 `Int` over `L` | 8,314 | 8,312 |
| `java/lang/invoke/MemberName` slot 4 `Int` over `L` | 50 | 50 |
| `java/util/Scanner` slot 4 `Int` over `L` | 2 | 2 |
| `java/util/Scanner` slot 3 `Int` over `L` | 2 | 2 |

The `HashMap` row's 8,314 → 8,312 is inside its own documented run-to-run spread
(±4 on a single probe against an unmodified binary); it is not a signal in
either direction at that resolution.

**Two brand-new cross-type WRITE rows, which is gap 3 firing on production
input** — the pre-fix binary reported neither, and both are `Object(None)` over
an `int`:

* `jdk/internal/math/FloatingDecimal$1` slot 0, from `FloatingDecimal$1.<init>`;
* `java/util/concurrent/locks/ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter`
  slot 0, from its `<init>` under `new ReentrantReadWriteLock(boolean)`.

`Properties` slot 3 — the row the design predicted this would surface — does
**not** appear, and the prediction was stale rather than wrong: L2 step 3 had
already routed both `native_props_init` and every reader through
`props_defaults_slot`, so nothing writes a null there any more. The shadow diff
still reports the slot (`model=_f3:Ljava/lang/Object; real=loadFactor:F`), which
is the point of having a per-class instrument as well as a per-access one.

### The finding: 23 slots where our model names a different field

This is the family the record calls **kind 5** and describes as unfindable by
census — "the census is a floor, and for reference-into-reference it is a floor
of zero". It is not any more. Live sites, from three small probes:

| class | slot | our model says | the image declares |
|---|---:|---|---|
| `java/lang/ThreadGroup` | 0 | `name:String` | `parent:ThreadGroup` |
| `java/lang/ThreadGroup` | 1 | `parent:ThreadGroup` | `name:String` |
| `java/lang/ThreadGroup` | 2 | `daemon:Z` | `maxPriority:I` |
| `java/lang/ThreadGroup` | 3 | `maxPriority:I` | `daemon:Z` |
| ~~`java/lang/Thread`~~ | ~~5~~ | ~~`contextClassLoader:ClassLoader`~~ | ~~`holder:Thread$FieldHolder`~~ **FIXED 08-05** |
| ~~`java/security/ProtectionDomain`~~ | ~~1~~ | ~~`permissions`~~ | ~~`classloader`~~ **FIXED 08-05** |
| ~~`java/security/ProtectionDomain`~~ | ~~2~~ | ~~`classloader`~~ | ~~`principals:[Principal`~~ **FIXED 08-05** |
| `java/security/CodeSource` | 1 | `certs:[Certificate` | `signers:[CodeSigner` |
| `java/io/BufferedWriter` | 0 | `out:Writer` | `writeBuffer:[C` |
| `java/lang/reflect/Field` | 4 | `type:Class` | `name:String` |

`ThreadGroup` is the clearest: **our model has both pairs transposed**, so a
positional write of the group's name lands on `parent` and vice versa — the
identical shape to the `ClassLoaders` defect L1 found by hand, in a class nobody
had looked at. Not accessed by these probes but reported by the diff:
`java/io/BufferedReader`, `InputStreamReader`, `OutputStreamWriter` (slot 0
`in`/`out` over `lock`/`writeBuffer`), `java/lang/reflect/Method` and
`Constructor` (slots 1/3/4/6), `java/util/Collections$SingletonMap` (`k`/`v`
over `keySet`/`values`). `ProtectionDomain` slot 3 went with slots 1 and 2 — **all three FIXED 2026-08-05**.

**Status of the list, end of 2026-08-05: TWELVE of twelve fixed.**

Seven were rotations — `ThreadGroup`, `Thread`, `ProtectionDomain`,
`CodeSource`, `BufferedReader`, `BufferedWriter`, `Collections$SingletonMap` —
each a separate change with its own A/B. The last five were not, and needed two
different answers:

* **`reflect/{Field,Method,Constructor}`** were shifted, not rotated:
  `AccessibleObject` declares TWO instance fields (`override`,
  `accessCheckCache`) and `Executable` adds two more (`parameterData`,
  `declaredAnnotations`) ahead of Method's and Constructor's own, and the models
  had none of them. Twenty disagreeing slots, all gone with a model edit — the
  58 raw slot accesses that made this look structural turned out to be by-NAME
  resolution plus extras anchored at `base + *_EXTRA_OFFSET_*` past
  `class_num_total_fields`. Three live sites stopped being reported (`Field`
  4/5, `Constructor` 4): each was a by-name read landing CORRECTLY on the image
  and flagged only because the model claimed a different field there.

* **`InputStreamReader` / `OutputStreamWriter`** could not be fixed by naming,
  because the names they claimed (`in`, `out`) do not exist on those classes at
  any index. The value the VM parks there has no home. That produced the
  instrument's third verdict, below.

### `_vmN` — the third model spelling

`SlotVerdict::VmInternal` (tag `VM`) reports a slot the model declares `_vmN`:
"the VM keeps its OWN value here" — an fd, a wrapped stream, a discriminator —
whenever a real field turns out to live at that index. It is silent when the
slot is anchored past the real layout, which is the shape a fix should reach.

It exists because the two spellings this design shipped with both lie about a
kind 3. Naming the slot after the real field makes the diff **agree with the
overlay**; leaving it anonymous makes the diff **say nothing**. The
`BufferedWriter` fix earlier the same day took the second option, and turned a
wrong NAME row into silence while the fd stayed exactly where it was. Relabelling
it `_vm0` brought `BufferedReader` slot 0 and `InputStreamReader` slot 1 into
the census with it — two overlays that had never been reported at all.

Final census: **139 → 124** unique disagreeing class+slot pairs. Twenty removed,
five added, and every one of the five is a kind-3 row that was previously
mislabelled or invisible. Six probe transcripts byte-identical across the A/B.

Also settled in passing, for L3: `java/util/Scanner`'s model is
`instance_fields(5)`, not 3, and slots 3 and 4 are `delimPattern` and
`hasNextPattern` — both real `java.util.regex.Pattern` references. That resolves
L3 step 3's "either the model grew or a different writer is involved" without
running a tracer.

### Verification

**No behaviour change with the flag off.** All six probe/mode transcripts are
identical pre/post once timestamps and the ephemeral TCP port are normalised
(the same two artefacts the L2 verification records). One `--jdk-only` census
run truncated at the `net` section on the first attempt and completed on retry —
a socket-bind stall on this host, not a regression; the retry is identical to
the pre-fix transcript.

**Injected violations.** A temporary block in `native_props_init`, behind
`CRATONVM_L4_INJECT`, performed the three accesses the design names on a real
`java.util.Properties` — slot 3 is the inherited `Hashtable.loadFactor` (a
`float`) and slot 2 is `Hashtable.threshold` (an `int`) — saving and restoring
the real values so the injection did not corrupt the run. Reverted before
landing. Verbatim, under `CRATONVM_DBG=overlay,overlay-all,overlay-nodedup`:

```
[L4-INJECT] gap 3: write Object(None) over a primitive slot (3=loadFactor:F)
[OVERLAY] suspect native set_field [cross-type+model-slot]: class=java/util/Properties slot=3
          value=Object(None) real_field_desc='F' model=_f3:Ljava/lang/Object; real=loadFactor:F verdict=TYPE
[L4-INJECT] gap 1: READ a slot the model and the image disagree about (3)
[OVERLAY] suspect native get_field [model-slot]: class=java/util/Properties slot=3
          value=Float(0.0) real_field_desc='F' model=_f3:Ljava/lang/Object; real=loadFactor:F verdict=TYPE
[L4-INJECT] gap 2: write Int into a real Int slot (2=threshold:I, type-checks)
[OVERLAY] suspect native set_field [model-slot]: class=java/util/Properties slot=2
          value=Int(4242) real_field_desc='I' model=_f2:Ljava/lang/Object; real=threshold:I verdict=TYPE
[L4-INJECT] done
```

The gap-2 line is the one to read twice: `Int(4242)` into a slot the image
declares `I`. There is no type mismatch anywhere in it, the pre-L4 predicate
passes it in silence, and it is reported.

**`overlay-nodedup` exists because of this run.** The first injection attempt
produced *nothing* for gap 2 — the per-site cap had already been spent on that
`(class, slot, write)` by `native_map_init` moments earlier, so the injected
violation was swallowed by the instrument's own volume control. That is the
shape of a guard that cannot fail, and it is the reason the token is not
optional decoration: `CRATONVM_DBG=overlay-nodedup` turns the cap off and gives
per-site counts instead of per-site presence.

Beyond the injection, **each gap also fires on production input, A/B'd against a
binary that is silent on the same input**: gap 3 by the two `Object(None)`-over-
`int` rows above, gap 1 by 70 read sites where there were none, and gap 2 by
`set_field java/lang/ThreadGroup slot=2 value=Int(10) real_field_desc='I'
model=daemon:Z real=maxPriority:I verdict=NAME` — an `Int` into a real `Int`
slot, type-checking exactly as the design said it would, reported anyway.

`cargo test -p cratonvm-classloading --lib shadow_layout` covers the diff itself,
including one test run against the **production** `java/util/Properties` model
rather than a fixture of one, so emptying the table fails a test instead of
turning the census silently green.

### What the detector still cannot see

Stated here rather than left to be discovered:

1. **An anonymous `_fN` model slot over a real reference field.** The model
   declares `Ljava/lang/Object;` and so does every reference field in the JDK,
   so there is nothing to compare. This is the residual half of kind 5 and no
   amount of re-running finds it. **The way to shrink it is to name more of
   `synthetic_stub_fields`** — every arm converted from `instance_fields(n)` to
   a named list turns n unfalsifiable slots into n checkable ones. That is the
   follow-up this lane recommends and does not do: naming a stub's fields is
   also a behaviour change (`set_field_by_name` starts resolving), so it wants
   its own change and its own A/B.
2. **An anonymous model slot over a real primitive is reported, but it is a map
   entry, not a defect.** `java/lang/Integer` slot 0 is `value:I` under an
   anonymous model and the VM writes an `Int` there entirely correctly. The
   rendered line shows `model=_f0:Ljava/lang/Object;`, so the weakness is
   visible on its face — filter on `verdict=NAME` (or `VM`) for the actionable
   set.
3. **Classes with no model at all.** The diff is driven by
   `synthetic_stub_fields`; a native that indexes a real JDK class with no arm
   in that table is outside this instrument entirely. The access-site cross-type
   check still covers it; nothing else does.
4. **Three probes is still not Spring Boot.** Every number above is a floor for
   the same reason it was before.
5. **A `_vmN` slot says a value has no home; it does not fix one.** The five
   `VM` rows are open kind-3 defects, filed in
   `docs/known-issues/jdk-only/files-newbufferedwriter-parks-an-fd-in-writebuffer.md`.
   Each wants a side table keyed by the object, or the index anchored past the
   real field count. Marking them countable is what this lane could do; only one
   of them (`Files.newBufferedWriter`) fires in the default build.
6. **Which is the reason a behavioural probe still earns its keep.** The
   overlays above are not yet observable from Java, so a probe cannot find them.
   `probes/ReaderWriterLayoutProbe` was written to be the thing that notices when
   they become observable — and on its very first run against Temurin 25.0.3 it
   found an unrelated live divergence anyway (`getEncoding()` returning the
   canonical charset name where the JDK returns the historical one, 24 charsets
   affected, fixed). A paired transcript diffed against the host JDK keeps paying
   for itself; see the L2 note on the same pattern.

### Volume, and why the two halves are counted differently

A cross-type report still prints on every occurrence, so the pre-L4 rows keep
their counts and an A/B against a pre-L4 binary compares like with like. A
model-slot-only report prints **once per (class, slot, read/write) per process**:
the complete list of disagreeing slots is already printed once per class by the
shadow-layout census, so the per-access line's only job is to name the native,
and one occurrence does that. Without the cap, `java/lang/String` slot 1 alone —
model reference, image `byte coder`, read on every string operation — buries the
run. `CRATONVM_DBG=overlay-nodedup` turns the cap off.

### Tokens

| token | what it does |
|---|---|
| `overlay` | the hunter, both halves, plus the per-class shadow-layout census |
| `overlay-all` | drop the `java.util.Map` suppression, and print the census for classes whose model *agrees* too |
| `overlay-bt` | Rust backtrace for each report, optionally filtered (`overlay-bt=ThreadGroup`) |
| `overlay-nodedup` | **new** — report every model-slot access, not the first per site |

### Housekeeping done here

`types/tests/flag-surface.txt` and both generated flag docs were **already out of
sync on dev** — seven flags from the interpreter-resolved-constant-pool lane were
declared in `INVENTORY` with no fixture row, so `flag_surface` and
`flag_docs_generated` were red before this change touched anything. Registering
`overlay-nodedup` meant regenerating those files anyway, so the seven were added
in the same pass and all four flag tests are green again.
