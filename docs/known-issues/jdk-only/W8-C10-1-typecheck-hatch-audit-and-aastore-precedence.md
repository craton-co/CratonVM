# Every fail-open hatch in `typecheck.rs`, audited — plus an inverted `aastore` exception precedence

**Status: MIXED. Two hatches CHANGED (see
`W7-101-aastore-interface-component-blanket.md`), one deliberately KEPT with its
reasoning, one residual measured and left, one defect NOMINATED (different
file).** Lane C10, 2026-08-12.

> **RENUMBERED 2026-08-13 (lane C18).** The sibling record this one leans on
> was filed as `W7-39` while `W7-39-jca-missing-algorithms.md` — filed 23 hours
> earlier the same day — already held that number. The aastore record is now
> **`W7-101-aastore-interface-component-blanket.md`**, and all 11 references in
> this file were rewritten. Any surviving `W7-39` you meet elsewhere means the
> **JCA** record. **Two Rust comments still cite the old path** and are
> NOMINATION N1 in `INDEX.md`: `vm/src/runtime/interpreter/typecheck.rs:782`
> and `vm/src/runtime/interpreter/tests.rs:91` — including the one this
> record's §"required for the tree to build green" hands over as literal text.

Oracle: HotSpot 25.0.3, Microsoft build 25.0.3+9-LTS, windows/x64. Probes:
`scratchpad/c10/Oracle.java`, `NestedNames.java`, `GenTable.java`,
`blanket.rs`, `EmptyList.java`. No CratonVM binary was built or run by this
lane; CratonVM "before" numbers marked EXECUTED came from the orchestrator's run
on the wave binary, everything marked PREDICTED is source reading.

---

## 1. Why audit the whole file

`W7-101` is one instance of a shape this project keeps finding: **a fail-open
guard written before its precise replacements existed, which then permanently
pre-empts them.** The guard does not fail; it succeeds at answering early, and
every more careful successor added below it becomes dead code with no build
error and no failing test.

That shape is not self-limiting — the same file had already accumulated a second
one (`W7-101` §3), and a third was found by another lane the same day
(`W8-C4-2`). So every early `return true` in the file was read and asked three
questions: what was it for, does something below it now do that job properly,
and would deleting it change any answer.

**The ones kept are the result this record exists for.** A hatch that has been
examined and deliberately retained, with the reasoning written down, is what
stops the next lane spending a day re-opening it.

## 2. The audit

`aastore_element_assignable`, in source order. "Oracle" is what HotSpot answers
for a value that reaches the arm.

| # | arm | what it is for | successor below it? | verdict |
|---|---|---|---|---|
| 1 | `array_descriptor_of` returns `None` | value is not an array / shape unreadable | none — this is the only thing that can answer | **KEPT** |
| 2 | `!array_desc.starts_with('[')` | defensive | unreachable: arm 1 guarantees the `[` | **KEPT** (dead, cheap, honest) |
| 3 | component is `Object`/`Serializable`/`Cloneable` | "these accept any reference" | yes, the whole machinery | **KEPT — but WRONG for two of the three.** §3. **CLOSED by lane C16**, `W8-C16-1` |
| 4 | array value with no descriptor | shape unreadable | none | **KEPT** |
| 5 | array value, component neither `L…;` nor `[…` | defensive | unreachable | **KEPT** |
| 6 | component neither `L…;` nor `[…` | defensive | unreachable | **KEPT** |
| 7 | `value_class_id == ClassId(0)` | comment says "unknown/synthetic class id" | **yes** — `get_class(..).is_none()` 30 lines below does exactly that | **CHANGED** (W7-101 §5) |
| 8 | `comp_id == ClassId(0)` | component name did not resolve | none | **KEPT** |
| 9 | `get_class(value_class_id).is_none()` | value's class not in the store | none — this IS the real version of arm 7 | **KEPT** |
| 10 | by-name superclass walk | split-loader same-name copies | subsumed by the new `is_assignable_to_name` call | **KEPT** as the allocation-free fast path; noted in-code as one check, not two |
| 11 | component is an INTERFACE | proxies / synthetics | **yes**, five of them | **DELETED** (W7-101) |
| 12 | `value_class_id >= 0x8000_0000` | synthetic lambda/proxy ids, absent from the hierarchy | none | **KEPT** |
| 13 | `$Proxy` / `AnnotationProxy` name test | runtime-acquired interfaces | none | **KEPT** |
| 14 | `class_chain_reaches_proxy_instance` | generated `$ProxyN` subclasses | none | **KEPT** |
| 15 | `synthetic_implements` | name-based synthetic relationships | none | **KEPT**, but see §4 |

Arms 12–15 are the populations arm 11's comment cited. They were unreachable
while arm 11 stood, and they are the reason deleting it is safe. `W7-101` §5 has
the full argument and the one thing that had to be ADDED to make it safe.

## 3. Arm 3 — kept, and measurably wrong on three rows

> **CLOSED 2026-08-13 by lane C16 —
> `W8-C16-1-serializable-cloneable-are-not-object.md`.** The pair this section
> asks for is what landed: `Object` split into its own arm, and the
> `Serializable`/`Cloneable` relationships made declarable by name in
> `synthetic_implements`, with a `ClassOrigin::CompatibilityStub`-scoped hatch so
> a fabricated class cannot produce a false `ArrayStoreException`. The
> below-the-line risk this section names was **sized** rather than argued: the
> stub table declares `Cloneable` on 2 classes where HotSpot has it on 81.
> `s10` is expected to split by mode, deliberately; read `W8-C16-1` §6 before
> calling that a partial fix. The rest of this section is left as filed — it is
> the reasoning that made the pair the right shape.

```rust
if component == "Ljava/lang/Object;"
    || component == "Ljava/io/Serializable;"
    || component == "Ljava/lang/Cloneable;"
{
    return true;
}
```

Only the first third is a correct rule. `Object[]` genuinely accepts every
reference. `Serializable` and `Cloneable` are ordinary interfaces for `aastore`
purposes, and HotSpot treats them that way — measured, `scratchpad/c10/Oracle.java`:

```text
Serializable[] <- Object      -> ArrayStoreException | java.lang.Object
Serializable[] <- Integer     -> OK
Serializable[] <- Integer[]   -> OK
Cloneable[]    <- Object      -> ArrayStoreException | java.lang.Object
Cloneable[]    <- Integer[]   -> OK
Cloneable[]    <- Integer     -> ArrayStoreException | java.lang.Integer
```

Three rows (`Serializable[] <- Object`, `Cloneable[] <- Object`,
`Cloneable[] <- Integer`) are admitted by this arm and denied by the oracle. The
asymmetry in the last pair is the tell: every ARRAY implements `Cloneable`, and
`Integer` does not.

### Why it is kept anyway

Not because it is defensible as written — it is not — but because the change has
a different risk profile from `W7-101`'s and this lane could not build or run:

* The machinery below WOULD answer it correctly in real-JDK mode: an array value
  still reaches `array_is_assignable_to`, which returns `true` for a
  `Serializable`/`Cloneable` target by its own rule, and a plain object reaches
  `is_subclass_of`, which finds `Integer → Number → Serializable`.
* But `synthetic_implements` — the last-resort fallback for classes with no real
  interface data — **has no `java/io/Serializable` or `java/lang/Cloneable`
  arm at all.** In synthetic-JDK mode a fabricated class stored into a
  `Serializable[]` would go from silently accepted to `ArrayStoreException`, and
  the predicate's documented contract is that it must never produce a FALSE
  `ArrayStoreException`.
* The reach is small in the other direction: `Serializable[]` and `Cloneable[]`
  are rare component types, whereas `Comparable[]`/`Runnable[]`/`Map.Entry[]`
  (which arm 11 covered) are not.

So: three wrong rows, bounded, versus an unmeasurable spurious-throw risk in a
mode this lane cannot exercise. **Splitting `Object` from the other two, and
adding `Serializable`/`Cloneable` arms to `synthetic_implements`, is the shape
of the fix** — it is a pair, like every other item in this file, and it wants a
lane that can run the synthetic-JDK gate. `RArrayStoreInterfaces` rows
`s08`/`s09`/`s10`/`s21`/`s22`/`s23` are the vector and are RED on CratonVM today
by construction (PREDICTED).

## 4. `synthetic_implements` — the substring blanket, and what W8-C4-2 leaves

Lane C4's nomination is APPLIED in this lane's commit (it is in a file C10
owns): `java/util/ImmutableCollections$Map*` and `$AbstractImmutableMap` are now
excluded from the `contains("Collection")` term, so `Map.of() instanceof
Collection` stops answering `true`.

This lane verified it rather than taking it, with a `rustc`-only probe —
`scratchpad/c10/blanket.rs`, built with plain `rustc -O` so it takes no cargo
build lock and writes into no target dir — over a 92-row oracle table generated
straight from HotSpot reflection (`GenTable.java`), so no oracle cell is
hand-transcribed.

**First, the instrument had to be corrected.** Scoring "rows where the predicate
disagrees with HotSpot" reports 43 wrong rows and makes the *correct* guard on
line 982 look like the largest defect in the file. It is not: `synthetic_implements`
is a last-resort fallback reached only after the real hierarchy check has
declined, and it can only ADMIT. A `false` from it means "no opinion", and 33 of
those 43 rows are `java/util/Collections$*` classes where the guard is declining
exactly as its comment says it should. **The two directions are not symmetric
and must never be summed.** Counting over-admissions only:

```text
over-admissions BEFORE              = 11
over-admissions AFTER (W8-C4-2)     = 7  (fixed 4, regressed 0)
over-admissions with SIMPLE-NAME    = 1
```

The probe asserts `before > 0` (or it could not go red and would prove nothing)
and `regressed == 0`.

**W8-C4-2 is sound and over-excludes nothing.** Independently checked by
enumerating every declared nested class of `java.util.ImmutableCollections` on
HotSpot and reading `Collection.class.isAssignableFrom` for each
(`NestedNames.java`): all four classes the new prefix excludes are
`isCollection=false, isIterable=false`. No green cell moves.

**But it closes 4 of 11, and the residual is the same mistake one level down.**
Every nested class of `java.util.ImmutableCollections` matches
`contains("Collection")` via the CONTAINER's name, whatever it is. Still
over-admitted after the fix:

```text
java/util/ImmutableCollections$StableMap                        <- a Map
java/util/ImmutableCollections$StableMap$StableEntry            <- an entry
java/util/ImmutableCollections$SetN$SetNIterator                <- an iterator
java/util/ImmutableCollections$MapN$MapNIterator ... (fixed)
java/util/ImmutableCollections$StableMap$StableMapEntrySet$LazyMapIterator
java/util/ImmutableCollections$ListItr                          <- an iterator
java/util/ImmutableCollections$HasStableDelegates               <- not a collection at all
java/util/ImmutableCollections$Access                           <- not a collection at all
```

`StableMap` is the one that matters most: it is a `Map`, it is reachable, and it
is wrong for exactly the reason `Map1`/`MapN` were.

**Not fixed here, and nominated instead** (§7 N3), because the general fix
changes the answer for every `java/util/*` name in the VM and this lane cannot
build. The general fix is measured, though: run the substring test on the
innermost SIMPLE name rather than the whole binary name, and refuse iterators.
That is 11 over-admissions → 1 on the same table. Naming container classes one
at a time is what produced this record's own subject.

## 5. Vector — `regression-suite/src/RArrayStoreInterfaces.java` (NEW, applied)

27 shapes, 108 checks. `PASS` on HotSpot with the JIT **and** under `-Xint`.

Why it exists beside `RArrayStoreTiers`, which already has four interface rows:
**a blanket allow gets every LEGAL row right for free.** With 4 rows, of which 2
are legal, a degenerate predicate scores 50% and reads like a partial defect. So
this fixture pairs every legal store with an illegal store into the SAME array,
and prints the two counts separately —
`illegal stores refused: N/12   legal stores admitted: N/15` — plus a hard
`DEGENERATE` divergence if *no* illegal store was refused. A green line can
never mean "nothing was checked".

It also asserts the fail-open populations that must KEEP passing: a dynamic
proxy into an array of an interface it was created with (`s25`), and an
annotation proxy into `Annotation[]` (`s26`). A fix that turns those red has
replaced one wrong answer with another.

Mutation-checked: emulating the interface blanket in pure Java (`if (BLANKET)
return;` in each illegal-store method) yields `AssertionError: 36 divergence(s)`
including the `DEGENERATE` line, so it can go red.

Messages are asserted only on the cold pass, per
`W7-40-tier-parity-fixtures-and-fast-throw.md`. The dynamic-proxy row's message
names a generated class whose number is not stable, so its message is skipped
and only its kind asserted.

**CratonVM: not run. PREDICTED red** before `W7-101`'s fix on `s01`–`s07` and
`s11`–`s12`, and red on `s08`/`s09`/`s10` until §3 is addressed.

## 6. A separate defect — `aastore` exception PRECEDENCE is inverted

**EXECUTED**, orchestrator run: `RArrayStoreTiers` `s15`
(`String[] as Object[]`, index 5, `<- Integer`) —

| | |
|---|---|
| HotSpot | `ArrayIndexOutOfBoundsException` |
| CratonVM | `ArrayStoreException` |

and it is wrong in the INTERPRETER, not only in compiled code.

JVMS §6.5 *aastore* fixes the order: `NullPointerException` if the array
reference is null, **then** `ArrayIndexOutOfBoundsException` if the index is out
of range, **then** `ArrayStoreException` if the value is not
assignment-compatible. Confirmed on the oracle
(`Oracle.java`: `String[] as Object[] idx 5 <- Integer -> ArrayIndexOutOfBoundsException`).

`vm/src/runtime/interpreter/opcodes.rs`, the `Aastore` arm, runs them
NPE → **ASE** → AIOOBE. The bounds check is not a check at all: it is the error
return of `set_array_element`, which happens *after* the covariance block. The
NPE half is correct — `pop_object_ref_ctx_with` runs first — which is why `s14`
passes and only `s15` fails.

**The JIT helper already has it right.** `jit_aastore` (`vm/src/jit/helpers.rs`)
does the null check, then `if index < 0 || index >= length` with an early
return, then the covariance check. So this is the mirror image of `W7-38`: there,
the interpreter was right and the compiled path skipped the check; here the
helper is right and the interpreter has the order wrong.

That matters for sequencing. Once `W7-38`'s codegen change routes `0x53` to
`jit_aastore`, `s15` becomes a **tier split in the opposite direction** —
AIOOBE in compiled code, ASE interpreted. Anyone reading that as a `W7-38`
regression will be looking at the wrong file.

Not fixable here (different file). Nominated in §7 N1.

## 7. Nominations

### N1 — `vm/src/runtime/interpreter/opcodes.rs`: bounds before store check

Hoist an explicit bounds check above the covariance block, matching
`jit_aastore`'s order. In the `Instruction::Aastore` arm, insert immediately
before the comment line `// JVMS §aastore covariance check:`:

```rust
            // JVMS §6.5 aastore fixes the order of the three checks:
            // NullPointerException (done above, by `pop_object_ref_ctx_with`),
            // THEN ArrayIndexOutOfBoundsException, THEN ArrayStoreException.
            // The bounds test used to be nothing but `set_array_element`'s error
            // return, which runs AFTER the covariance block below — so an
            // out-of-range index with an incompatible value reported
            // `ArrayStoreException` where HotSpot reports
            // `ArrayIndexOutOfBoundsException` (measured: `RArrayStoreTiers` s15).
            // `jit_aastore` already had this order; the interpreter did not.
            // See docs/known-issues/jdk-only/W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md
            {
                let alen = shared.mem.heap.array_length(array_ref) as i32;
                if index < 0 || index >= alen {
                    return Err(RuntimeError::aioobe(index, alen).into());
                }
            }
```

Types checked, not assumed: `RuntimeError::aioobe(index: i32, length: i32)`
(`types/src/error.rs:1372`), and `index` in this arm comes from
`stack.pop_int()?`, so it is already `i32` — no cast either side. Its message
builder is `out_of_bounds_message::check_index`, which produces HotSpot's
`Index 5 out of bounds for length 1`, matching the oracle text
`RArrayStoreTiers` already asserts for `s15` in its cold pass. This lane could
not compile it; that is the one thing left to confirm.

Verify with `RArrayStoreTiers` `s15` under `--nojit`. `s14` (NPE, which precedes
the bounds check) and `s16` (primitive store) must not move, and `s01`–`s05`
must keep throwing `ArrayStoreException` — an in-range illegal store must not
start reporting AIOOBE.

### N2 — `vm/src/runtime/interpreter/tests.rs`: the test that pins the deleted blanket

**This is required for the tree to build green.** `W7-101`'s deletion makes
`aastore_refuses_a_real_mismatch_and_still_fails_open_where_it_must` fail: it
asserts the blanket directly. The file is not C10's.

OLD (exact literal):

```rust
    // Documented lenience 1: an INTERFACE component. Proving a value implements
    // an interface is unreliable here (dynamic/annotation proxies, synthetic
    // classes implement them at runtime), so the predicate declines to throw.
    assert!(
        aastore_element_assignable(&shared, iface_arr, beta_obj),
        "an interface component must fail open",
    );
```

NEW:

```rust
    // NO LONGER a documented lenience. An interface component used to `return
    // true` unconditionally, which made this the assertion that pinned the
    // blanket in place; HotSpot 25.0.3 throws `ArrayStoreException` for
    // `Runnable[] <- String` and `Comparable[] <- Object`, and the predicate
    // permitted both. An unrelated concrete value must now be REFUSED against
    // an interface component exactly as against a concrete one.
    // docs/known-issues/jdk-only/W7-101-aastore-interface-component-blanket.md
    assert!(
        !aastore_element_assignable(&shared, iface_arr, beta_obj),
        "AastoreBeta implements nothing, so storing it into an AastoreIface[] \
         must be refused — an interface component is not a reason to fail open",
    );

    // …and the fail-open population the blanket was WRITTEN for is still served,
    // now by the arm that can actually tell a proxy from an `Object`: the
    // `$Proxy` name test below `is_subclass_of`. Without this assertion the one
    // above would also pass against a predicate that had started refusing
    // everything with an interface component.
    assert!(
        aastore_element_assignable(&shared, iface_arr, proxy_obj),
        "a $Proxy-named value must still fail open against an interface component",
    );
```

The doc comment above the test ("fails open along five separate arms", "the
lenient arms alone are what a degenerate `true` satisfies") stays accurate:
`proxy_obj` into `alpha_arr` and `proxy_obj` into `iface_arr` are both still
lenient arms, and the refusals are still the control.

### N3 — `vm/src/runtime/interpreter/typecheck.rs` (C10's own file, deliberately NOT taken this wave)

> **TAKEN 2026-08-13 by lane C16 —
> `W8-C16-2-synthetic-implements-simple-name.md`.** Landed with one addition
> this nomination did not have: the term must also be a camel-case WORD, which
> was measured to be free (25 further over-admissions closed, zero admissions
> lost). Re-measured on a table built from the runtime image rather than the 92
> rows below — 3,462 classes, 20,772 name×target cells — where the numbers are
> **410 → 50** over-admissions, 16 correct admissions lost, 0 regressions. The
> single survivor this nomination predicted
> (`…UnmodifiableEntrySetSpliterator`) is closed by the cursor rule and does
> not appear in the residual.

Generalise `synthetic_implements`' substring arm to the innermost SIMPLE name,
closing the 7 residual over-admissions in §4 instead of naming container classes
one at a time. Measured on the 92-row table: 11 → 1 over-admissions (the
survivor is
`Collections$UnmodifiableMap$UnmodifiableEntrySet$UnmodifiableEntrySetSpliterator`,
whose simple name contains "Set"). Held back because it changes the answer for
every `java/util/*` name in the VM and no lane in this wave can build — it wants
its own measurement against the strict corpus, not a same-wave add-on. Probe and
both variants are in `scratchpad/c10/blanket.rs`.

### N4 — `regression-suite/run.sh` (not C10's file)

Register the new fixture. Append to the `CORE_CLASSES` value:

OLD (end of the `CORE_CLASSES` line):

```
 RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

NEW:

```
 RJdkStrictMath RJdkByteOrder RJdkIntrinsics RArrayStoreInterfaces"
```

Note `W8-C4-2` §5 nominates an append to the same line for
`RImmutableFactoryTypes`; if both land, the line takes both names.

**Land it with your eyes open:** `RArrayStoreInterfaces` is PREDICTED RED on
CratonVM until `W7-101`'s fix is in the binary, and rows `s08`/`s09`/`s10` stay
red until §3 is addressed. Hold the registration if the suite must stay green;
do not weaken the fixture to land it.

## 8. `Collections.emptyList` — comment corrected, arm kept

`vm/src/runtime/interpreter/native_override.rs` (C10's file). The force arm's
comment claimed:

> "The real Collections.emptyList() returns the class's pre-built static
> singleton. During the Brave bootstrap that slot can retain a polluted
> ArrayList, so use the registered constructor-backed empty-list native instead
> of exposing that stale shared state."

Only the first sentence survives contact with the native it routes to.
`native_collections_empty_list` (`native-collections/src/lib.rs`) *begins* with
`collections_empty_singleton(ctx, "EMPTY_LIST")`, which is a
`get_static_field(java/util/Collections, EMPTY_LIST)` — it READS the very slot
the comment says this arm exists to avoid. A polluted slot would be handed
straight back. The arm cannot deliver the protection it advertised.

The hazard was real and was **fixed at its source, elsewhere**:
`ensure_collections_empty_singletons` used to seed `EMPTY_LIST` with a mutable
synthetic `ArrayList`, so `emptyList() instanceof ArrayList` was true,
kotlin-reflect's shaded protobuf `SmallSortedMap.ensureEntryArrayMutable` skipped
its replacement step on the strength of that, and mutated the process-wide
singleton. It now seeds the real immutable `Collections$Empty*` instances.

What the arm actually delivers today is the native's SECOND half: a fabricated
empty list when `EMPTY_LIST` is not yet initialised. That fallback diverges from
the oracle on all three properties (`scratchpad/c10/EmptyList.java`):

```text
class            = java.util.Collections$EmptyList     (fallback: ArrayList)
add("x")         = UnsupportedOperationException       (fallback: ACCEPTED)
two calls same   = true                                (fallback: fresh each call)
```

**Kept, comment rewritten in place**, because deleting it is a PAIR and neither
half is useful alone:

* the triple is not in `RETIRED_SHADOW_TRIPLES`
  (`native-api/src/retired_shadow.rs`), so dropping the arm leaves the
  registered `Bridge` winning by ordinary dispatch and changes nothing;
* adding the table entry alone leaves this arm forcing the native over the
  bytecode.

And there is a prior question nobody has asked: **whether this arm decides
anything at all.** `resolve_step1_native` dispatches the registry hit before
`force_native_over_real_jdk_bytecode` runs — which is exactly why the
twelve-shape forced-native `java/lang/String` policy sitting 30 lines above this
one turned out to be measured inert and was deleted. The same question applies
here and is the cheapest thing to measure first. It is written into the code
comment so the next reader gets it without this record.
