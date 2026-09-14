# H24-3 — `RJdkFunctionCombinators` is down to ONE family, `Comparator`, and ten of the eleven stand-in names are already dead: measured by invocation count, not by grep

**Status: DIAGNOSED TO CLOSURE, NOT FIXED — the remedy is in
`native-collections/src/lib.rs`, which lane H23 owns and this lane must not
edit.** Lane H24, 2026-08-21, on the prebuilt `C:/craton/cratonvm-r8.exe`
(clean build at `025780ff7`, which **includes** `H19-1`'s landed fix —
`165eceb73` is an ancestor of `025780ff7`, checked). Oracle HotSpot **25.0.3+9**.

Every claim is **MEASURED** (this lane ran it today) or **ARGUED** (read it).

This record answers `H19-1` N1 — *"run `RJdkFunctionCombinators` and name the
next failure"* — and replaces `H15-3` §2.3's eleven-row grep table, which that
record correctly flagged as **ARGUED, not measured**, with a measurement. The
answer is smaller than either record feared: **one fix, not five.**

---

## 1. The next failure, by name

**MEASURED**, Compatible mode, `r8`:

```
CK RJdkFunctionCombinators predicate rightRuns=2 order=[L, R]
CK RJdkFunctionCombinators consumer=aAbB three=123 bi=x7|
CK RJdkFunctionCombinators function=8 compose=10 biRight=0
CK RJdkFunctionCombinators maxBy=abcd minBy=ab
CK RJdkFunctionCombinators primitive intRight=0 ic=a3b3
Exception in thread "main" java/lang/AssertionError:
    Comparator.comparingInt is the fabricated compatibility class
    java.util.Comparator$Native
        at RJdkFunctionCombinators.comparatorCombinators(...java:392)
```

`H19-1` moved the vector from its **third** family to its **sixth**. Five `CK`
lines now print where one used to. The families that previously could not be
reached at all — `Function`, `UnaryOperator`, `BiFunction`, `BiPredicate`,
`BinaryOperator.maxBy/minBy`, and the whole primitive block — all pass their
`notFabricated` screens now.

## 2. The eleven names, measured

`H15-3` §2.3 listed eleven stand-in names from a grep and said, honestly, *"this
is one fix or it is up to five, and I cannot tell you which without a build."*
It does not need a build. `--dump-native-registry` (schema 5) carries an
`invocations` count per row, so the question *"is this stand-in reached?"* is
directly readable off a run of the vector itself.

**MEASURED**, `--dump-native-registry` taken **during** the Compatible run of
`RJdkFunctionCombinators`:

| stand-in class | rows still registered | **invocations** |
|---|---:|---:|
| `Predicate$$Lambda$And` | 1 | **0** |
| `Predicate$$Lambda$Or` | 1 | **0** |
| `Predicate$$Lambda$Negate` | 1 | **0** |
| `Consumer$AndThen` | 1 | **0** |
| `Function$AndThen` | 1 | **0** |
| `Function$Compose` | 1 | **0** |
| `Function$Identity` | 4 | **0** |
| `UnaryOperator$Identity` | **0** | 0 |
| `BinaryOperator$MaxBy` | **0** | 0 |
| `BinaryOperator$MinBy` | **0** | 0 |
| **`Comparator$Native`** | 2 | **8** |

Across the whole combinator family, **3 of 29 registered rows fired**, and all
three are `Comparator`: `comparingInt` (2), `naturalOrder` (1), and
`Comparator$Native.compare` (8).

**Three rows of `H15-3`'s table are stale outright** — `UnaryOperator$Identity`
and `BinaryOperator$MaxBy`/`$MinBy` have no registration left at all, retired by
`b81aae8fc` ("seven function stubs deleted"). Seven more are registered but not
reached. One is live.

### 2.1 The half of this that is NOT a measurement, stated plainly

**A zero is only informative for a name the run actually exercised.** The run
dies at line 392, so any zero belonging to a call site after that line means
"not reached by this run", not "dead".

For the ten zeros above this does not bite, and that is checkable from the
vector's own line numbers: `Predicate` is asserted at `:104-121`, `Consumer` at
`:181-214`, `Function` at `:229-246`, `UnaryOperator` at `:253`, and
`BinaryOperator.maxBy/minBy` at `:301-306` — **all before 392, and all of them
printed their `CK` line and passed.** So those ten were exercised and did not
fire. That is a real negative.

The zeros I am **NOT** claiming anything from are the other `Comparator` rows —
`reversed`, `reverseOrder`, `comparing`, `comparingLong`, `comparingDouble` and
the four `thenComparing*`. They sit at `:393-461`, after the throw. Their zeros
are pure reach.

## 3. `owns_slot` — the trap checked, and three live instances of it found

The brief requires checking `owns_slot` before "fixing" any row, because
`H14-1` measured 162 triples registered more than once with only the owning copy
live, and `H22` showed a retirement that scored as a win while promoting 16
condemned bodies into service.

**MEASURED, for the family that matters: all 14 `Comparator` rows have
`owns_slot: true` and `overwrote: null`.** There is no shadowed second
registration. Retiring `register_comparator_natives` retires the reachable
bodies, which is what makes this family the honest target.

The trap is nonetheless real in this same census, and it lands on **files this
lane owns**:

| triple | shadowed copy (`owns_slot: false`) | live copy (`owns_slot: true`) |
|---|---|---|
| `Function$Identity.apply` | `phases_late/streams.rs:3482` | `native-builtins/src/lib.rs:41327` |
| `BinaryOperator.apply` | `phases_late/streams.rs:2826` | `native-collections/src/lib.rs:26739` |
| `BiConsumer.accept` | `phases_late/streams.rs:2748` | `native-collections/src/lib.rs:26727` |

The first row **confirms `H19-1` §2 by the method that record asked for** — it
reasoned that `identity()`'s `streams.rs` registration decides nothing because
`lib.rs` registers it again later, and asked the next lane to confirm with
`owns_slot` rather than by reading the two call sites. Confirmed: `lib.rs`
owns the slot. A lane that "fixed" the `streams.rs` copy would have measured
exactly nothing.

*(Whole-registry aside, **MEASURED**: 862 `(class, name, descriptor)` triples
appear more than once in the schema-5 dump of a Compatible boot. This is **not**
a contradiction of `H14-1`'s 162 — that count was taken over a different
population and this lane did not reconcile the two denominators. Reported as a
number this lane measured, not as a correction.)*

## 4. Why the remedy is a retirement, and why it is not this lane's to make

The whole `Comparator` factory family lives in **one** registrar,
`register_comparator_natives` at `native-collections/src/lib.rs:34090`, already
tagged `NativeKind::SyntheticStub`. That tag is exactly why the strict arm is
green: **MEASURED**, `--jdk-only` on `r8`, `PASS RJdkFunctionCombinators (452
checks)`, byte-identical to HotSpot. The stand-ins are dropped there and the
real bytecode runs.

The real bytecode is present, and the registry says so rather than a comment:
**MEASURED**, all 12 `java/util/Comparator` rows carry
`real_declaring_method: {declared: true, has_code: true, loaded: true}`. Every
retired factory lands on a real default method with a real `Code` attribute.

**The minting surface is closed by the same edit.** `Comparator$Native` is
allocated only by `make_comparator` (`:33598`), and **every** caller of it —
`:34218`, `:34226`, `:34315`, `:34355`, `:34399`, `:34406` — is the body of a
`native_comparator_*` function registered by that one registrar. The only other
reference is a unit test at `:68681`. So retiring the registrar makes the class
genuinely unreachable, and the two `Comparator$Native` rows (`compare`,
`writeReplace`) go dead rather than getting promoted. **That is the `H22` check,
and this family passes it.**

`native-collections/src/lib.rs` is **lane H23's file**. This lane owns
`lang_invoke.rs`, `phases_late/streams.rs` and `classloading/**` and made no
edit here.

## 5. The risk H23 must price — this is NOT a free retirement

**ARGUED**, and it is the reason this record does not say "just delete it":
retiring the factories is a **path switch**, not a deletion.

`comparator_compare` (`:33607`) branches on a tag read from the synthetic
receiver's field 0. With the factories gone, `Comparator.naturalOrder()` returns
the real JDK `Comparators$NaturalOrderComparator` singleton, the tag read finds
no tag, and the call falls through to the generic arm at `:33726`:

```rust
return match ctx.invoke_virtual(comparator, "compare",
                               "(Ljava/lang/Object;Ljava/lang/Object;)I", &[a, b]) {
```

So **every `TreeMap`/`TreeSet`/`sort` that today reaches `CMP_TAG_NATURAL_ORDER`
and runs the Rust `natural_compare` would instead run real `compareTo`
bytecode.** That is a much wider blast radius than the vector, and it is
throughput-relevant on exactly the collection paths `H0-3` calls "not a leaf".
The correctness direction is favourable (it is what HotSpot does, and it also
makes `writeReplace`'s serialization substitution unnecessary because the real
singleton is already serializable) — but *favourable* is not *measured*.

**Retiring a workaround is a path switch; audit the new path.** The audit here
is a full `SUITE=all` plus the collection-heavy app suites, not this one vector.

## 6. The rest of the retirement surface, so it is not discovered late

Two more sites name the stand-in and would be left dangling. Neither blocks the
retirement — a bootstrapped class nobody mints is inert — but both belong in the
same change:

* `vm/src/vm/vm_init.rs:1671` — `ensure_bootstrap_compat_class(…,
  "java/util/Comparator$Native", 3)`, with a comment at `:1666` explaining that
  it exists so an implicit `checkcast` to `java/util/Comparator` succeeds.
* `native-api/src/no_image_receiver.rs:196` — the stand-in is in the
  no-image-receiver list.

## 7. What this record does NOT establish

* **It does not close the vector, and does not predict that one edit closes
  it.** The `Comparator` rows after line 392 have never been reached in
  Compatible mode by anything. `nullsFirst`/`nullsLast` (`:454`, `:461`) and the
  null-rejection census (`:560-568`) are entirely unmeasured there. The honest
  statement is: **one family blocks it now; whether a seventh lies behind the
  sixth is unknown**, and the same "run it and report the next failure by name"
  discipline applies to whoever lands this.
* **No arm was run against any change to this family** — this lane made none.
* The 862 duplicate triples were counted, not adjudicated.

## 8. NOMINATIONS

* **N1 — H23 (or whoever owns `native-collections/src/lib.rs`) retires
  `register_comparator_natives`'s twelve factory rows**, then re-runs
  `RJdkFunctionCombinators` and reports the next failure by name. §4 supplies
  the reachability proof and §5 the risk that must be measured alongside it.
* **N2 — delete the seven registered-but-unreached stand-ins** (`Predicate$$…`
  ×3, `Consumer$AndThen`, `Function$AndThen`, `Function$Compose`,
  `Function$Identity` ×4 rows). **MEASURED zero invocations** while their call
  sites were exercised, so this is cosmetic-plus-ratchet, not behavioural — but
  it is exactly the "deleting a native whose class is gone" follow-up
  `H15-3` §2.4 deferred, and it is now backed by a count instead of an argument.

  **This lane deliberately did NOT do it, even though four of the rows are in
  its own `streams.rs`.** `native-builtins/tests/stub_ratchet.rs` carries exact
  counts with `SLACK: usize = 0` — `BASELINE_SYNTHETIC_STUBS` 1626/1615 and
  `MEASURED_TOTAL_REGISTRATIONS` 13225/12857, per management-feature config.
  Any deletion moves all four constants, and re-freezing them requires *running*
  the test, which requires a build this lane may not do. Landing the deletion
  without the re-freeze leaves the gate red for everyone. Whoever takes N2 must
  take the re-freeze in the same commit, on a MEASURED and fully attributed
  delta — the rule `e6d642f3b` landed for.
* **N3 — `H15-3` §2.3's table should be marked superseded.** Three of its
  eleven rows describe registrations that no longer exist. A grep-derived table
  in a record is a snapshot, and this one is nine days stale in a directory
  where "a triage page is stale the day after it is written" is already a
  standing note.
* **N4 — `invocations` from `--dump-native-registry` should be the standard
  instrument for "is this stand-in reached?"** Both `H15-3` and `H19-1` reached
  for a grep and both flagged the result as unreliable. The measurement costs
  one flag on a run that was happening anyway, and it distinguishes *registered*
  from *reached* — which is the distinction every one of these records needs.
  Its one limitation is the one in §2.1: pair a zero with the line number of the
  call site that would have produced it.
