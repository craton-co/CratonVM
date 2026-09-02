# W7-77 — the four guarded slot maps, and what each guard is actually worth

Status: the four rows W7-69-read-side-alias-instrument.md filed as *"guarded, but
worth a look"* and *"synthetic-only"* are re-derived, dispositioned and gated.
**No map is renumbered.** Three of the four are correctly guarded and the right
change was to make the guard's removal loud; the fourth (`java/time/Month`) had
no runtime guard at all — only a registrar gate — and now has one.

Nothing here was built or run: this lane writes code, docs and probes. Every
claim about the real JDK layout is `javap -p` against Eclipse Adoptium
25.0.3.9 on this Windows host, counted transitively over the superclass chain
with `static` excluded — the same oracle and convention as
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md, W7-59 and W7-69.
Every claim about CratonVM is source-level and says so. The one thing that WAS
executed is HotSpot itself: `probes/GuardedSlotMapProbe.java` and its
swap-simulation both ran on `java` and their transcripts are in §6.

Branch `fix/month-stringjoiner-method-slot-maps-20260812`.

> **SOURCE-VERIFICATION BANNER — 2026-08-12, triage pass (A28). §5.3's
> disposition stands and BOTH of its stated reasons are false. Do not quote
> either of them.**
>
> §5.3 declines to renumber `METHOD_LEGACY_SLOT_*` on two grounds: (a) *"the
> guard means these indices are only ever applied to the fabricated mirror, where
> they ARE the layout"*, and (b) a renumber *"would break `test_utils.rs`'s
> `MockNativeContext`, which maps field names onto exactly these slots and is the
> oracle for the synthetic-mode tests"*. Read against the tree, neither holds.
>
> **(a) The fabricated mirror's layout is not the legacy map — it is the REAL
> map.** `ClassManager::synthetic_stub_fields`' `"java/lang/reflect/Method"` arm
> (`classloading/src/class_manager.rs:13231`) is `pad_to(vec![…], 15)` over
> eleven **named** fields, and its own comment (`:13222`–`:13230`) records that
> they were deliberately aligned to the image — `Executable`'s two on top of
> `AccessibleObject`'s two, *"so their own fields start at index 4, not index 1"*.
> The result reproduces §1's `javap` table exactly for 0..10:
>
> | | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
> |---|---|---|---|---|---|---|---|---|---|---|---|
> | real JDK 25 **and** the fabricated stub | `override` | `accessCheckCache` | `parameterData` | `declaredAnnotations` | `clazz` | `slot` | `name` | `returnType` | `parameterTypes` | `exceptionTypes` | `modifiers` |
> | `METHOD_LEGACY_SLOT_*` | `clazz` | `name` | `returnType` | `modifiers` | `slot` | — | `override` | `parameterTypes` | `callerSensitive` | `exceptionTypes` | `annotations` |
>
> The two agree on `exceptionTypes`(9) and on nothing else — which is §3's
> *"11 of 12 disagree"* verdict, correct as stated, but the class it disagrees
> with is **both** classes, not just the real one.
>
> **And the guard does not select the legacy map for the fabricated mirror.**
> `method_class_has_named_layout` (`native-builtins/src/lang_class.rs:8363`) is
> `resolve_field_index_by_class_id(class_id, "clazz").is_some()`. The fabricated
> stub **declares `clazz`**, at index 4. So the predicate answers `true` for the
> fabricated mirror exactly as it does for a real one, and the fabricated mirror
> takes the **named** path. The legacy indices are reached only by a receiver
> whose class declares no `clazz` at all — which is neither shipping mode's
> `java.lang.reflect.Method`. That is the foreign-receiver case W7-49 §8 says no
> width-derived helper can make safe, and it is why the disposition is still
> right: **not because the map is correct for someone, but because it is correct
> for nobody and reached by nobody.** A future lane should be told that the
> honest change here is a deletion behind a gate, not a renumber and not a
> defence.
>
> **(b) The mock is not an oracle for this; it is a third copy of the legacy
> map.** `mock_jdk_field_slot` (`native-builtins/src/test_utils.rs:197`) is
> `clazz`→0, `name`→1, `returnType`→2, `modifiers`→3, `slot`→4, `override`→6,
> `parameterTypes`→7, `callerSensitive`→8, `exceptionTypes`→9, `annotations`→10,
> `parameterAnnotations`→11, `annotationDefault`→12 — **byte-for-byte
> `METHOD_LEGACY_SLOT_*`**. And `MockNativeContext::resolve_field_index_by_class_id`
> (`:2112`) falls through to `mock_field_slot` for any field a test did not
> declare with `set_declared_fields`, where the doc at `:228` records that
> `mock_jdk_field_slot` is made to **outrank** the fabricated model for exactly
> this family. So under the mock a `Method` mirror's `clazz` is at 0; under the
> VM, in both modes, it is at 4. The mock and the legacy map agree with each
> other and with nothing the VM produces.
>
> Consequence beyond this row, and it is the reason to write it down: **any test
> that asserts a slot INDEX on a `Method`/`Field`/`Constructor` mirror is
> measuring `mock_jdk_field_slot`, not CratonVM's layout.** That is the
> mock-name-to-slot-table trap this project already keeps a record of, met here
> in the one family the mock is deliberately allowed to shadow. The gates §6.1
> lists for this row are text scans over source and are unaffected; it is the
> behavioural tests around the mirror that inherit the mock's numbers.
>
> **Verified true and not to be re-derived:** `declare_slot_map(` has **eight**
> production call sites — `native-io/src/lib.rs:5691`,
> `native-collections/src/lib.rs:30409`, `jdk25_concurrency.rs:2058`,
> `lang_reflect.rs:2021`, `phases_early.rs:11523`,
> `phases_late/concurrent.rs:8376` and `:8656`,
> `phases_late/net_channels.rs:78` — so §7 item 2's corrected denominator holds.
> `verify_declared_slot_maps` **has callers** (`vm-cli/src/main.rs`,
> `vm/src/vm/vm_init.rs`), so §7 item 2's CLOSED note holds. There really are
> **two** `MONTH_FIELD_VALUE` maps (`util_time.rs:5523`, `phases_early.rs:11423`),
> so §2.1 holds. `SJ_SYNTHETIC_SLOT_*` are renamed and span 0..4, and
> `class_manager.rs:12367` fabricates `java/util/StringJoiner` at **5** — the one
> row of this record where allocation width, fabricated width and slot map all
> agree, which is why §5.2's "the map is for a different class" reading is right.
> §3.1's `Thread` table also holds, but see W7-73's triage banner: the fabricated
> `java/lang/Thread` declares **eight** fields, not the five that record's sources
> still claim, and this record's `THREAD_SYNTHETIC_NUM_FIELDS` = 6 is a third
> value again.

## 1. The oracle, and what it says

Four layouts, re-derived before anything was touched. The task's instruction to
do this was not ceremony — W7-69 was wrong or incomplete on three of the four
rows, in three different ways (§3).

**`java.time.Month`** declares no instance fields of its own. Its whole
transitive layout is `java.lang.Enum`'s:

| slot | field | type |
|---:|---|---|
| 0 | `name` | `java.lang.String` |
| 1 | `ordinal` | `int` |
| 2 | `hash` | `int` |

**`java.util.StringJoiner`** (extends `Object`), 7 instance fields:

| 0 | 1 | 2 | 3 | 4 | 5 | 6 |
|---|---|---|---|---|---|---|
| `prefix` | `delimiter` | `suffix` | `elts` (`String[]`) | `size` (int) | `len` (int) | `emptyValue` |

**`java.lang.reflect.Method`**, chain `AccessibleObject` → `Executable` →
`Method`, 20 instance fields:

| 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|---|---|---|
| `override` | `accessCheckCache` | `parameterData` | `declaredAnnotations` | `clazz` | `slot` | `name` | `returnType` | `parameterTypes` | `exceptionTypes` |

| 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19 |
|---|---|---|---|---|---|---|---|---|---|
| `modifiers` | `signature` | `annotations` | `parameterAnnotations` | `annotationDefault` | `root` | `genericInfo` | `methodAccessor` | `hash` | `callerSensitive` |

**`java.lang.Thread`**, 19 instance fields; the first six are what matter here:

| 0 | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| `eetop` (long) | `tid` (long) | `name` | `interrupted` | `contextClassLoader` | `holder` |

The oracle was checked against W7-69's own reproduced table before it was
trusted: `java/lang/Enum` `name`(0)/`ordinal`(1) reproduce, and so does the
19-wide `Thread` that W7-69 §4.3 cites as "5 vs 19".

## 2. Liveness and the winner, and how it was decided

Not by indentation, and not by brace-counting by eye. A Rust lexer-lite scanner
(line comments, nested block comments, char-literal vs lifetime, raw strings,
and backslash-at-EOL line continuations inside string literals) was **validated
against ground truth before any of its answers were used**: final depth 0 at
EOF, scanned line count equal to the file's, and every `fn NAME` it found
verified to appear on the line it claims. On the five files this lane walked —
`phases_early.rs` (24,914 lines), `native-collections/src/lib.rs` (61,168),
`lang_class.rs` (26,195), `native-builtins/src/lib.rs` (43,547),
`util_time.rs` (6,280) — it reports depth 0, exact line counts and **zero fn
misattributions**. Every line it named was then re-read with `sed`.

| row | registrar that wins | reached from | mode |
|---|---|---|---|
| `java/time/Month` | `register_phase52_time_enums` (`phases_early.rs:11492`) ← `register_phase52_natives` | `register_synthetic_overrides` at `lib.rs:23869` | **synthetic only** |
| `java/util/StringJoiner` | `register_string_joiner_natives_with_category`, from both `register_collections_natives` (Bridge) and `register_annotation_overrides` (SyntheticStub) | `vm_init.rs` calls `register_collections_natives` in **both** arms; the Bridge copy is **dropped** in real-JDK mode by `set_drop_real_layout_synthetic(true)` | both, different winner per mode |
| `java/lang/reflect/Method` | `create_method_object`, published from `register_wp2_1_natives` | `register_annotation_overrides` ← `register_essential_natives_with_shims` (`lib.rs:19122`) | **both** |
| `java/lang/Thread` | `register_jdk25_concurrency_natives` (`lib.rs:24110`); the *consumer* is `vm_exec.rs::thread_start` | `register_synthetic_overrides` | registrar synthetic-only, consumer both |

`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`
(`lib.rs:21412`) and `vm_init.rs:1837` calls `register_builtins` only under
`if config.use_synthetic_jdk`. That mode also **skips boot-classpath discovery**
(`vm_init.rs:1191`), which is the load-bearing half of the Month argument in §4.

### 2.1 A registrar the census missed, and it is dead

**`java/time/Month` has TWO slot maps, not one.** `util_time.rs:5484` declares
its own `MONTH_FIELD_VALUE = 0` with its own `alloc_month`, and
`register_t25_natives` (`util_time.rs:5827`–`5831`) registers five triples on
it: `of`, `getValue`, `length(Z)I`, `maxLength`, `minLength`.

**All five are overwritten.** `register_synthetic_overrides` calls
`register_t25_natives` at `lib.rs:23852` and `register_phase52_natives` at
`:23869`, `register()` is last-write-wins, and `register_phase52_time_enums`
registers every one of those five triples plus seven more. Not one body in that
`util_time.rs` section executes in a real run; its only remaining callers are
the file's own `#[cfg(test)]` block.

Two things follow, and both are recorded in the source rather than only here.

1. It is dead **by call order, not by construction**. Swapping those two lines
   in `register_synthetic_overrides` makes it the winner. A renumber applied to
   one map and not the other leaves the tree with two answers for slot 0 — which
   is the failure mode this lane was told to avoid, arriving from an unexpected
   direction.
2. W7-69 §4.3's *"**Nothing is dead** in this population"* does not hold. Its
   census classified one `MONTH_FIELD_VALUE`; there are two, identically named,
   in the same crate. That is the same shape as `alloc_time_synthetic`, which
   is likewise declared twice in `native-builtins` (`lib.rs:30593` and
   `util_time.rs:133`) with the same signature and the same body — the exact
   trap the task warned about, found in the same crate on the same day.

## 3. Where W7-69 was wrong, and where it was right

Reported in full, per the method note, because four censuses in this area were
each wrong about some rows.

| W7-69 claim | verdict |
|---|---|
| `Method` — "11 of 12 slots" disagree | **CORRECT.** Only `EXCEPTION_TYPES`(9) agrees. |
| `StringJoiner` — slots 0/1 swapped, 4 `emptyValue`→`size` | **CORRECT** as far as it goes; see below for what it counted as agreeing. |
| `Month` — slot 0 is `Enum.name`, a String reference | **CORRECT.** |
| `Month` — an escape is "a bogus pointer for the collector to mark and move" | **WRONG.** §4. |
| `Thread` — "1 slot" (`isVirtual`(5)→`holder`) | **UNDERSTATED.** §3.1. |
| `Month` census row is the only one | **INCOMPLETE.** §2.1: there are two maps and the other is dead. |
| "Nothing is dead in this population" | **FALSE** for `java/time/Month`. |
| every `lib.rs` line number it cites | **STALE.** §3.2. |
| `phases_early.rs:11352`, `lang_class.rs:7625`, `jdk25_concurrency.rs:196` | **CORRECT**, all three. |

### 3.1 `Thread` is four disagreeing slots, not one

W7-69 listed `SYNTHETIC_THREAD_VIRTUAL_SLOT`(5) alone. The adjacent
`THREAD_FIELD_*` run in the same file is a **separate** `const` run naming no
class in its own comment, so the comment-scraper never reached it. Against the
real layout:

| slot | map says | real class has |
|---:|---|---|
| 0 | `name` | `eetop` |
| 1 | `priority` | `tid` |
| 3 | `target` | `interrupted` |
| 4 | `contextClassLoader` | `contextClassLoader` ✓ |
| 5 | `isVirtual` | `holder` |

Four of five disagree. Slot 4's agreement is deliberate — the 2026-08-05
alignment that moved the virtual flag 4 → 5 precisely so the two conventions
stop overlapping, and whose story is in `SYNTHETIC_THREAD_VIRTUAL_SLOT`'s own
doc comment. This is **not a new defect**: same fabricated map, same class-side
`eetop` witness deciding whether any of it may be applied. It is recorded
because "the census listed one" reads as "the other four agree", and they do
not.

`StringJoiner` has a smaller version of the same thing: the census counted three
disagreeing slots (0, 1, 4) and treated slot 3 (`ELEMENTS` vs `elts`) as
agreeing. The names differ, and so do the *types* — the map stores an
`ArrayList` where the real field is a `String[]`. It is counted as agreeing here
too, for continuity, but a reader should know the agreement is nominal.

### 3.2 Every `lib.rs` line number in W7-69 is stale, by a constant

| W7-69 | actual today | drift |
|---|---|---|
| `register_builtins` :21210 | :21401 | +191 |
| `register_synthetic_overrides` :21221–23978 | :21412–24193 | +191 / +215 |
| `register_t25_natives` :23637 | :23852 | +215 |
| `register_phase51_natives` :23651 | :23866 | +215 |
| `register_phase69_natives` :23730 | :23945 | +215 |
| `SJ_FIELD_*` :29479 | :29619 | +140 |

The drift is *constant within a file*, which is the signature of the file having
grown above the cite, not of the cites having been wrong when written. W7-69
landed today; `lib.rs` gained 215 lines above `register_t25_natives` since. The
three citations into files this lane did not otherwise touch are all exact.
**Line numbers in this record will rot the same way** — the function names are
the durable part.

## 4. Is `Month`'s Int-in-a-reference collector-visible? **No — on either layout.**

This is the question the task singled out, and the answer contradicts W7-69 §6.
The fact it offered — that native-allocated objects use the legacy layout, whose
`for_each_ref_slot` matches on the stored `Value` tag — is true but is **not the
reason**, and it does not have to extend to objects the native did not allocate,
because a second, independent screen catches those.

**The compact layout is the default.** `types::field_layout::compact_ref_fields_enabled()`
returns `true` unless `CRATONVM_COMPACT_REF_FIELDS` is set to `0`/`false`/`off`/`no`.
So reasoning only about the legacy 16-byte-cell arm answers the minority case.

Both arms screen the tag before anything reaches the collector:

* **Compact.** `Heap::set_field` routes through
  `types::field_layout::write_compact_field`, whose `FieldStorageKind::Reference`
  arm is `Value::Object(Some(r)) => ptr`, `Value::Object(None) => 0`, `_ => 0`.
  A `Value::Int` takes the `_` arm and stores **zero**. The Int is dropped; the
  slot reads back null. `gen_heap::for_each_ref_slot`'s compact arm then reads
  offset 0, sees `raw == 0`, and skips it.
* **Legacy.** `for_each_ref_slot`'s final arm is
  `if let Value::Object(Some(r)) = ptr::read(s as *const Value)`. An `Int` cell
  does not match and is never visited.

So on both layouts this is a wrong **answer**, not heap corruption. There is no
path in this tree by which the write becomes a pointer the collector marks or
moves.

**What the escape would actually cost** is quieter and still worth preventing.
On a real `Month` under the default compact layout the write nulls `Enum.name`
on a *shared enum constant*, and `getValue()` then reads `Object(None)`, whose
`.as_int()` is `None`, so the `unwrap_or(1)` at every read site answers
**January for every month of the year**. That is a silently wrong date, spread
across `Month`, `MonthDay` and `OffsetDateTime.getMonth`.

**Can it escape?** Not without a code change. `register_phase52_time_enums` is
reachable only from `register_synthetic_overrides`, `vm_init.rs` calls that only
under `config.use_synthetic_jdk`, and that mode skips boot-classpath discovery
entirely (`vm_init.rs:1191`), so no real `java.time.Month` is loadable on the
arm where the natives exist. The thing that keeps a *code change* honest is a
test, not a runtime predicate — hence `month_registrars_stay_synthetic_only`
(§5) — and the runtime witness is defence in depth behind it.

## 5. Disposition per row

### 5.1 `java/time/Month` — **strengthen the guard** (there was none)

W7-69 filed this as synthetic-only. It was *not* guarded: the only thing between
`Value::Int` and `Enum.name` was which registrar had been called. All 17 access
sites now funnel through two functions in `phases_early.rs`:

* `month_set_value` / `month_alloc` (write), `month_value` (read);
* both consult `month_slot0_is_synthetic`, a **class-side witness** —
  `resolve_field_index_by_class_id(class_id_of_object(obj), "name")` — keyed on
  the receiver's `ClassId` rather than on the name `java/time/Month`, because
  the name-based lookup answers `None` for "two loaders define it" as well as
  for "nobody does", and this predicate must not read the first as the second;
* one funnel each rather than a witness copy-pasted 17 times, per
  `reference_convert_the_idiom_not_the_sites`.

Same remedy shape as `Thread`'s `eetop` witness and `Method`'s
`has_named_layout`. **Not renumbered**: on the fabricated stub, slot 0 genuinely
*is* the value, so a renumber would break the only receiver that exists.

**Mode:** synthetic only. Compatible mode never registers these natives, and
nothing in this change is reachable from the real-JDK arm.

The dead `util_time.rs` twin is documented in place, not deleted — deleting is
tempting (W7-69's own precedent says a superseded row is free to delete) but the
two bodies are *not* byte-identical: their `IllegalArgumentException` messages
differ (`"Invalid value for MonthOfYear: {m}"` vs `"…(valid values 1 - 12): {m}"`).
Deleting is still safe, since the phase52 message is the one that ships, but it
is a separate change from this one and wants its own justification.

### 5.2 `java/util/StringJoiner` — **strengthen the guard**, and stop the name lying

The map is **not wrong — it is for a different class.** It describes the
fabricated 5-slot stub (`_f0.._f4`), which really does have that layout. Both
that stub and JDK 25's class answer to the binary name `java/util/StringJoiner`,
so no `SlotMap` can separate them by name and no renumber can serve both. The
census row is meant to **stay**.

* Renamed `SJ_FIELD_*` → `SJ_SYNTHETIC_SLOT_*` (24 sites, compiler-checked, zero
  behaviour change in either mode). `SJ_FIELD_PREFIX = 1` read as
  "StringJoiner's prefix is field 1"; it is `delimiter`. A *rename* carries none
  of a renumber's risk — the compiler checks it — which is why it is the right
  tool here.
* Documented the guard precisely: `sj_real_layout` is a **class-side,
  all-or-nothing** witness over all seven real field names, and the six entry
  points plus the one private helper that depend on it are now listed by name in
  the source. `sj_read_elements` is the one reader that touches a constant
  without asking, and it is named as the deliberate exception rather than
  silently excluded — an unexplained exclusion rots.

**Mode:** the rename touches both, behaviourally neither. In real-JDK mode the
`Bridge` copy is dropped at registration by `set_drop_real_layout_synthetic`
(`registry.rs:6559`), which is a *registry-level* drop and stronger than the
runtime `if`; the surviving `SyntheticStub` copy takes the real-layout arm.
Compatible mode sees no behaviour change.

### 5.3 `java/lang/reflect/Method` — **strengthen the guard; do not renumber**

The guard is correct and is the standing remedy: `method_class_has_named_layout`
asks `resolve_field_index_by_class_id(class_id, "clazz")` — a class-side witness
asking the right question ("does this class HAVE a named field table") rather
than the wrong one ("did the named writes land"), which its own in-place comment
records as having been a corruption bug that made Byte Buddy report
`public abstract int int.value()`.

**Deliberately not renumbered**, and this is the row where the task's warning
bites hardest. A renumber fixes nothing reachable — the guard means these
indices are only ever applied to the fabricated mirror, where they ARE the
layout — and it would break `test_utils.rs`'s `MockNativeContext`, which maps
field names onto exactly these slots and is the oracle for the synthetic-mode
tests. That is precisely *a renumber that fixes one reader and breaks another
that agreed with the old map*.

**Mode:** both, behaviourally neither — the change is a `SlotMap`, a comment and
two tests.

### 5.4 `java/lang/Thread` — **already the exemplar; publish and gate it**

`vm_exec.rs::thread_start`'s `eetop` witness is the best-documented instance of
this remedy in the tree, including the record that its predecessor
(`num_slots() >= 5`) was wrong because every real `Thread` satisfies it. Nothing
about it needed changing. What it lacked was a machine-readable link and a test.
Both added; §3.1's three extra disagreeing slots recorded in place.

**Mode:** registrar synthetic-only, and that scope is right rather than a
limitation — in real-JDK mode the witness refuses the fabricated read outright,
so a sweep row there would be noise. In synthetic mode the sweep asks the
question that matters: does the fabricated `java/lang/Thread` still declare what
these constants believe? That is exactly the drift that put the virtual flag at
slot 4 until 2026-08-05.

### 5.5 All four now publish a `SlotMap`

`MONTH_SLOT_MAP`, `SJ_STUB_SLOT_MAP`, `METHOD_LEGACY_SLOT_MAP`,
`SYNTHETIC_THREAD_SLOT_MAP` — the machine-readable link W7-59 §6 asked for, each
handed to `declare_slot_map` from a registrar, each `const` data.

Each states **what the native believes, not the real layout.** A map that
publishes the correct answer sweeps clean and measures nothing; the point of the
declaration is that `verify_declared_slot_maps` reports the disagreement. A test
(`the_published_maps_state_the_belief_not_the_truth`) exists solely to stop the
next reader "fixing" the row by editing the declaration to agree with `javap`,
which would silence the census without changing a line of the code that reads
the slots.

**No `CRATONVM_*` flag was added.** `CRATONVM_DBG_LAYOUT_ALIAS` is reused, so
none of `types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md` or `docs/config/flag-inventory.md` needs a change and
`cargo test -p cratonvm-types` is unaffected. `HEADER_SIZE` is not a factor at
any site touched here and is not mentioned outside a symbolic reference.

> **VERIFIED AGAINST A BINARY 2026-09-02.** §6.1 says "This lane cannot run
> `cargo`", so all ten predicates were re-implemented outside the tree and run
> there. The real ones have now been run, on a build from this tree:
>
> ```text
> cargo test -p cratonvm-native-api --test guarded_slot_maps   11 passed, 0 failed
> cargo test -p cratonvm-native-api --test read_alias_coverage 12 passed, 0 failed
> ```
>
> **Eleven, not the ten §6.1 describes** — the file grew a test after this record
> was written. That is the same count drift every record in this campaign has
> shown; the assertions are what was verified, and the count is noted rather than
> treated as a finding.
>
> A re-implementation passing is weaker evidence than it looks: it shares the
> author's reading of the predicate, so it can only fail where the author already
> suspected a failure. Running the tree's own copy is what closes that gap, and
> §6.1's own framing — "a mirror is not a compiler" appears in F34-1 for the same
> reason — says so.
>
> Unchanged: §6.1's caveat that **all four rows are unobservable from Java
> today**. What passes is the guard, not a behaviour a probe could reach.

## 6. Proving the RED

### 6.1 The gates

Ten tests in `native-api/tests/guarded_slot_maps.rs`, one per link so a break
names which link broke. **All four rows are unobservable from Java today** —
each guard holds on every receiver the tree can currently produce — so what is
asserted is the guard, not a behaviour a probe cannot reach.

This lane cannot run `cargo`. Every predicate is a text scan, so all ten were
re-implemented outside the tree and run against the tree and against one
mutation each. **All ten GREEN on the unmutated tree; all nine mutations RED on
their own gate**, with no unexplained collateral:

| gate | mutation | result |
|---|---|---|
| month-funnel | `if !month_slot0_is_synthetic(..)` → `if false` | RED |
| month-registrar | a second `register_phase52_natives(registry)` call | RED |
| sj-entries | `sj_real_layout(ctx)` → `SJ_NOPE(ctx)` in `native_sj_init_full` | RED |
| sj-witness | drop `len` from `sj_real_layout` | RED |
| method-gate | a `METHOD_LEGACY_SLOT_CLAZZ` write hoisted above the gate | RED |
| method-readers | `if method_object_has_named_layout(..)` → `if false` | RED |
| thread-eetop | witness → `Some(header.num_slots() as usize)` | RED |
| published | delete `declare_slot_map(&…METHOD_LEGACY_SLOT_MAP)` | RED |
| belief | `(SJ_SYNTHETIC_SLOT_DELIM, "delimiter")` → `"prefix"` | RED |

The tenth, `fn_body_is_delimited_by_braces_not_by_column`, is ground truth for
the scanner the other nine depend on, and is asserted before any of them runs.

### 6.2 The simulation paid for itself, twice

Same rule as W7-69 §5.1 — *a gate never seen to fail is the most common wasted
effort* — and the same dividend.

* **The thread gate was RED on the untouched tree.** Its first predicate took a
  1,200-byte window from the first mention of `SYNTHETIC_THREAD_VIRTUAL_SLOT`.
  That first mention is a **comment 1,606 bytes above** the witness, so the
  window never reached it. A gate that fires on an unmutated tree gets deleted,
  not investigated — exactly how W7-69 lost its own gate 6. The shipped
  predicate anchors on `let is_virtual_synthetic = {` and delimits the block by
  **braces**. Recorded in the test itself, because the lesson is "do not guess
  an extent when the language gives you one", not "use a bigger window".
* **The Method gate carried a latent CRLF dependency.** It reconstructed
  per-line byte offsets as `l.len() + 1` to decide whether an access sat above
  the gate. `str::lines()` strips a trailing `\r`, so on a CRLF checkout — which
  this one is; `git` warns on every commit here — that undercounts by one per
  line and the verdict would have depended on `core.autocrlf`. It splits the
  body on the gate now and skips comment lines.

### 6.3 The probe, and why every line of it is NO-CHANGE

`probes/GuardedSlotMapProbe.java`. Only `StringJoiner` can express any of this
from Java; the other three rows are reachable only on the fabricated-class path,
where the disputed map IS the layout. **Every line is expected NO-CHANGE and the
probe says so in its own header.** The observable for a guarded row is the row
leaving the read-side census, not a behaviour change; the probe's job is to be
the behavioural half of the regression gate for the day a guard is removed.

Every expected value was **measured** on the host oracle — `java -version` =
OpenJDK 25.0.3+9-LTS (Temurin) — never guessed. 20/20 PASS.

The vacuous shape here is real and specific: the swapped map still produces a
perfectly well-formed joined string, so `isNotEmpty()` passes against it. So
**that each assertion discriminates was measured too**, by re-rendering every
case under the swapped map (legacy `<init>` writes delim→slot 0, prefix→slot 1,
suffix→slot 2; real class reads those as prefix/delimiter/suffix) and diffing
against the oracle:

| case | HotSpot | under the swapped map | caught |
|---|---|---|---|
| `sj.empty.toString` | `<PRE>[SUF]` | `-DELIM-[SUF]` | yes |
| `sj.two.toString` | `<PRE>A-DELIM-B[SUF]` | `-DELIM-A<PRE>B[SUF]` | yes |
| `sj.three.toString` | `<PRE>A-DELIM-B-DELIM-C[SUF]` | `-DELIM-A<PRE>B<PRE>C[SUF]` | yes |
| `sj.delimOnly.toString` | `p,q` | `,pq` | yes |
| `sj.empty.length` | 10 | 12 | yes |
| `sj.three.length` | 27 | 25 | yes |
| **`sj.delimOnly.length`** | **3** | **3** | **NO** |

Twelve of the thirteen StringJoiner assertions catch the swap. The one that does
not is kept — a stuck `size`, the *other* half of this map's disagreement, does
move it — and is labelled at its own call site, because an unlabelled
non-discriminating assertion sitting among discriminating ones is how a suite
starts being trusted for something it does not check.

## 7. What this lane did not resolve

1. **Nothing was built or run in CratonVM.** The gates' predicates were
   simulated, not executed by `cargo`; the probe's expected values are HotSpot
   measurements, and it has not been run under `cratonvm --real-jdk` or
   `--jdk-only`. Both should be NO-CHANGE, and that prediction is the record's,
   not a measurement.
2. **`verify_declared_slot_maps` still has no caller** — W7-69 §7.2, unchanged.
   Four more maps are now published to a sweep nobody invokes. Choosing its
   trigger needs a build. Until then the four rows' `SlotMap`s are checked only
   by `every_guarded_row_publishes_its_slot_map`, which proves they are wired,
   not that the sweep ever runs.
   **CLOSED 2026-08-12 by W7-90-slot-map-sweep-caller.md**, which wired two
   triggers (the launcher's post-`main` teardown, and the three self-terminating
   natives) and gated them. This row's four maps contribute 15 of the 29
   predicted census rows; the two synthetic-only ones can only be swept in
   synthetic mode, because their registrars are.
   **RE-VERIFIED 2026-08-12: the wiring is in the tree** — `vm-cli/src/main.rs`
   `:4281`, `vm/src/vm/vm_init.rs:7965`, `native-builtins/src/lang_system.rs`
   `:137` / `:1354` / `:2628` / `:2664`, with links 7–9 of
   `native-api/tests/read_alias_coverage.rs` holding it. The sentence above the
   CLOSED note is kept as written and struck by it; HANDOFF-20260812.md still
   repeats the pre-W7-90 claim and is stale on this row.
   **The denominator moved.** `declare_slot_map(` now has **eight** call sites,
   not seven — W7-88 added `SSC_P58_SLOT_MAP`
   (`java/nio/channels/ServerSocketChannel`, synthetic-only, four disagreeing
   slots). This row's four maps contribute 15 of **33**, and the real-JDK-mode
   figure is unchanged at 24 because every map added since has been
   synthetic-only. W7-90 §4.6.1 and §4.7 carry the corrected arithmetic.
   **One door is still open and it is the one that matters for this row's two
   synthetic-only maps' siblings**: a Surefire fork leaves through four
   `ForkedBooter` triples that never reach `System.exit`, so neither trigger
   fires there — W7-90 §2.2.1.
3. **The dead `util_time.rs` Month surface is documented, not deleted.** §5.1.
   **Re-examined 2026-08-12 by the lane that owns that file, and the disposition
   is DELIBERATELY UNCHANGED.** The surface is `MONTH_FIELD_VALUE`
   (`util_time.rs:5523`), `alloc_month` (`:5526`) and five bodies (`:5532`,
   `:5548`, `:5557`, `:5576`, `:5590`) registered at `:5866`–`:5870`; all five
   triples are still overwritten by `register_phase52_time_enums`, so §2.1's
   finding reproduces exactly. Three reasons not to delete, in the order they
   decided it:
   * It is dead by call **order**, and the deletion's whole safety argument is
     that order. A source-only lane cannot re-establish the order after removing
     the thing that made it observable, and there is no gate on it — W7-77 added
     `month_registrars_stay_synthetic_only`, which asserts the registrars' mode,
     **not** which of the two wins.
   * The `#[cfg(test)]` block at `:6105`–`:6170` is the only executable coverage
     of the month-length arithmetic anywhere in this crate — ten tests over
     leap/non-leap February, 30- and 31-day months and the `of()` range refusal.
     Deleting the production bodies deletes those with them, trading a documented
     dead row for a real loss of cover, and this lane cannot run `cargo` to
     confirm nothing else picks it up.
   * Nothing is gained. The row is already inert in both shipping modes, the
     hazard (two answers for slot 0) is a hazard only if someone swaps two lines
     in `register_synthetic_overrides`, and the in-place comment at `:5498`
     onwards states that and points at the funnel a revival would have to go
     through.
   **What would actually close it** is not a deletion: it is a source gate
   asserting `register_t25_natives` is called *above* `register_phase52_natives`
   in `register_synthetic_overrides`. That is the claim the deadness rests on and
   the only one nothing currently checks. It belongs in
   `native-builtins/tests/`, which is another lane's directory.
4. **`Month`'s witness is unmeasured for cost.** It adds one
   `resolve_field_index_by_class_id` per Month read and write. `java.time.Month`
   is not on any hot path this tree measures, but "not measured" is not "free",
   and if a date-formatting workload ever shows up in a profile this is a named
   place to look.
5. **The `StringJoiner` row cannot leave the census while the fabricated stub
   shares the real class's binary name.** `SlotMap.class` is a string; the two
   classes are not distinguishable by it. Closing that needs the sweep to key on
   something else — a `ClassId`, or a "is this class fabricated" predicate
   `NativeContext` does not have, which is the same gap W7-69 §7.5 names for
   `Unknown`.
   **Re-verified 2026-08-12 and STANDING, not stale.** `SJ_STUB_SLOT_MAP` is
   still declared with `class: "java/util/StringJoiner"`
   (`native-collections/src/lib.rs:30147`) and `SlotMap.class` is still
   `&'static str` (`native-api/src/read_alias.rs:439`), so the limit is
   structural rather than an oversight. It is worth being explicit that this row
   is **not** a defect and must not be "fixed" by editing the declaration: W7-90
   §6.4 states the general form — the sweep reports that a map disagrees with a
   class, never whether the code holding the map is reached with that receiver,
   so a printed row is a measurement and **guarded is not clean**. This row is
   the case where that distinction is load-bearing in both directions at once,
   because both classes really exist and each is right about itself.
6. **`W7-69`'s remaining unguarded rows are untouched here** —
   `jdk/internal/vm/Continuation` (5 slots) and
   `java/util/concurrent/ForkJoinPool` (2), both LIVE and both unguarded, are
   §6's first entries there and another lane's. This lane owned the guarded four.
