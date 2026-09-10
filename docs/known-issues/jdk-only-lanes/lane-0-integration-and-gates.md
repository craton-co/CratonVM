# Lane 0 — integration, shared gates, and `Class`/`Module`

**Owner: the integrating lane. Read this page before opening any other lane
page — it defines the ownership boundary every other lane depends on.**

The campaign goal is *remove all synthetic bridges shadowing real bytecode from
`--jdk-only` mode*. Eight sibling lanes retire shadows; this lane owns the cells
they all have to touch, and answers the only question that cannot be
parallelised: **what the number is**.

Authorities this page does not restate:

- [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) — the method, the four
  retirement preconditions (§7), the instrument traps (§4), and the **landing
  protocol (§5), which is the whole gate set and must not be shortened**.
- [`../stub-ratchet.md`](../../contributing/stub-ratchet.md) — why a ratchet constant is taken as a
  before-number and never computed by arithmetic.

---

## 1. The goal's population is 5,549 rows, not 9,386

Measured 2026-09-10 from `--dump-native-registry --explain-jdk-only` on the
current tree. Of 10,895 registrations, 9,386 are *eligible* — they own their
slot and their effective kind is `Bridge`, which is what the retirement
mechanism can act on at all. But eligible is not the goal:

| bucket | rows | eligible | in the goal? |
|---|---|---|---|
| **A** image method declares `Code` | 4,753 | **3,851** | **yes — §1.4 shadow** |
| **B** inherits `Code` from a supertype | 1,895 | **1,698** | **yes — §1.4 shadow** |
| C declared but abstract | 1,408 | 1,282 | no — no door dispatches it |
| D image method is `ACC_NATIVE` | 776 | 693 | **no — §1.5 says `Bridge` is CORRECT** |
| E no such class in the image | 1,340 | 1,229 | no — nothing to shadow |
| F class present, method absent | 723 | 633 | no — matches nothing |

**The goal's population is A + B = 5,549.** The other 3,837 eligible rows
cannot be retired by yielding, because there is no real bytecode behind them:

- **D (693) is out of scope permanently.** Contract §1.5 *requires* an
  `ACC_NATIVE` method to bind to a `Bridge`. Retiring one is a defect, not
  progress. `java/lang/Class.initClassName` is the worked example — it sits
  beside `getName`, which *was* a shadow, and stays `Bridge` deliberately.
- **C (1,282) is dead weight, not a shadow.** An instance-method native on an
  abstract class or interface is dispatched through no door. Deleting these is
  worth doing and is *not* a retirement; it must not be counted as one.
- **E (1,229) and F (633)** are compatibility-mode registrations for classes the
  JDK image does not carry (`org/apache/`, `io/netty/`, `org/springframework/`,
  `cratonvm/internal/`, most of `java/util/stream/`). Under `--jdk-only` they
  shadow nothing.

**Do not report progress against 9,386, and do not report it against the 1,414
"native-won cases" figure either** — that is a per-corpus-run *dispatch* census
over one workload, a different measurement with a different denominator. The
campaign denominator is 5,549.

## 2. Ownership table

Ownership is a **prefix set**, except for lane T which owns **whole registrars**.
Sets are disjoint; the totals below reconcile to 5,549 exactly.

| lane | scope | shadows | classes | reg sites |
|---|---|---|---|---|
| **L0** (this page) | `java/lang/Class` + `Class$*`, `ClassValue`, `ClassFrameInfo`, `Module*`, `java/lang/module/` — **not** `ClassLoader*` | 104 | 8 | 104 |
| **LT** | *whole cross-cutting registrars* (see §3) | **1,100** | 87 | **57** |
| **L1** | `java/util/` (less `concurrent/`), `java/text/`, `sun/util/`, `java/time/` | 963 | 98 | 726 |
| **L2** | `java/lang/` remainder, `java/math/` | 434 | 68 | 303 |
| **L3** | `java/lang/reflect/`, `jdk/internal/reflect/`, `sun/reflect/`, `java/lang/invoke/` | 251 | 36 | 246 |
| **L4** | `java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign` | 1,110 | 131 | 615 |
| **L5** | `java/util/concurrent/`, `jdk/internal/misc/`, `sun/misc/`, `java/lang/Thread*`, `jdk/internal/vm/` | 405 | 23 | 345 |
| **L6** | `java/net/`, `sun/net/`, `javax/net/`, `java/security/`, `sun/security/`, `javax/crypto/`, `javax/security/`, `jdk/net/` | 819 | 90 | 663 |
| **L7** | `java/lang/ClassLoader*`, `jdk/internal/loader/` **+ the bootstrap failure triage** | 47 | 12 | 45 |
| — | **UNOWNED, frozen** | 316 | 83 | 291 |
| | **TOTAL** | **5,549** | **631** | **3,395** |

**A prefix is a string, and `java/lang/Class` is a prefix of
`java/lang/ClassLoader`.** The first cut of this table was computed that way
and silently gave L0 all 27 `ClassLoader` rows plus, before the lane-T pass,
`ClassNotFoundException` and `ClassCastException`. L0's row above is now the
explicit set and L7's carries the `ClassLoader` rows that were never L0's
business. If you add a prefix anywhere, check what else it is a prefix of.

**The 316 unowned rows are frozen, not unassigned-by-accident.** They are
`jdk/internal/foreign/layout` leftovers, `java/beans`, `sun/java2d`,
`javax/management`, `sun/management`, `java/sql`, `java/awt/image`, `jdk/jfr`.
A lane that wants one **claims it by amending this table in its own commit**,
which serialises the claim through this file. Do not retire an unowned row.

## 3. Lane T exists because 1,100 rows come from 57 call sites

A pure prefix split is *wrong* for this codebase, and the census says so.
`native-builtins/src/lang_misc.rs:3416-3568` is one function,
`register_throwable_subclass_natives`, looping `THROWABLE_FAMILY_CLASSES` (61
classes) times ~12 methods:

```text
 158 rows / 60 classes   lang_misc.rs:3416   (the constructor table)
  61 rows / 61 classes   lang_misc.rs:3503   getMessage
  61 rows / 61 classes   lang_misc.rs:3510   getLocalizedMessage
  ... eleven such call sites ...
  53 rows / 53 classes   lib.rs:42662
  53 rows / 53 classes   lib.rs:42668
```

Its 61 classes span **seven lanes'** prefixes — `ClassCastException` is L0's,
`ExecutionException` is L5's, `IOException` is L4's, `SSLException` is L6's.
Splitting one registrar eight ways would have eight lanes editing one `for`
loop, each measuring a seventh of one behaviour change. So:

> **The unit of work for a cross-cutting registrar is the registrar, and lane T
> owns it whole.** While lane T holds a registrar, no prefix lane may retire any
> triple that registrar produces — even one inside its own prefix set.

Regenerate the cross-lane registrar list from a fresh dump rather than trusting
the snapshot above.

**The trap this lane inherits:** a class-parameterised registrar is invisible to
the source-scanning drift gate, which counts `register(` call sites — 61 rows
behind one call site read as one row, and rows lost behind a helper refactor
read as *good news*. Lane T's movements must be proven with the paired registry
census, never with the scanner.

## 4. Shared cells: who may edit what

| file / cell | owner | rule for other lanes |
|---|---|---|
| `native-api/src/retired_shadow.rs` — **your own** `RETIRED_SHADOW_L<N>_TRIPLES` | the lane | fill your array literal only |
| same file — `triple_is_retired_shadow()` chain | **L0** | pre-created; never edit |
| same file — `RETIRED_SHADOW_PREFIXES` | the lane | append **your** prefix in the same commit as your first entry — see below |
| same file — the N-way disjointness test | **L0** | never edit; it must fail if you collide |
| `native-builtins/tests/stub_ratchet.rs` — the three `BASELINE_*` constants | **L0** | **never edit.** Report your measured delta in your commit message |
| `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — **rows** | the lane | keyed class+name+descriptor, disjoint by construction |
| same file — the header note | **L0** | never edit |
| `regression-suite/bridge-ratchet.sh` | **L0** | never edit |
| [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) | **L0** | propose via your lane page |
| `apps/probes/L<N>*.java` | the lane | namespace your probes; `apps/` is gitignored, so `git add -f` |

### How a lane adds its table without touching another lane's work

`triple_is_retired_shadow` is a chain of `||` arms over sorted tables, and the
disjointness test is a hand-unrolled cascade. Both grow with the number of
tables, so a lane adds:

1. its own `RETIRED_SHADOW_L<N>_TRIPLES`, with the doc comment carrying its
   measurement;
2. one `||` arm in the predicate;
3. one arm in `the_two_tables_are_disjoint`;
4. its own sortedness, reachability and held-families tests.

Steps 2 and 3 are one line each in a shared function, which is small enough
that two lanes rarely collide and a collision is a trivial merge.

**An earlier draft of this page promised nine empty tables pre-created up
front.** That was dropped after building the real thing: an empty table needs a
sortedness test that trivially passes, a reachability test with nothing to
reach, and a name in the registry with no measurement behind it -- eight
placeholders that assert nothing and that a reader has to check are genuinely
empty rather than genuinely finished. The cascade being O(n2) in the source is
the honest cost, and it is paid one line at a time by the lane that benefits.

### Prefixes are the exception: they stay narrow and lane-owned

An earlier draft of this page said L0 would pre-add every lane's prefixes,
arguing that a prefix with an empty table is provably inert. The inertness claim
is true — the list is only an early-out for the binary search, so a wider list
plus an empty table answers `false` for the same inputs. **The conclusion drawn
from it was still wrong**, and `a_prefix_alone_retires_nothing` says why in its
own comment:

```rust
// HELD by the arm, and the prefix list does not admit it — belt and
// braces, because a widening of that list must not silently re-retire
// what RClassUnloadSweep rejected.
assert!(!triple_is_retired_shadow("java/lang/ref/Reference", "clear", "()V"));
```

The narrowness is a *second* guard, deliberately redundant with the tables.
`java/lang/ref/` is absent on purpose, and `sun/nio/` is admitted only as
`sun/nio/fs/` and `sun/nio/ch/` because a package-scoped dial sweep scored
34/36 against retiring it. Pre-adding a broad `java/lang/` or `sun/nio/` would
keep every current test green **and** delete that guard for the next reader.

So: **each lane appends its own prefix, in the same commit as its first entry
there.** The list is short and appends land in different places, so the conflict
risk is small — and forgetting is caught loudly, because an entry outside every
prefix makes the predicate answer `false` for a row that is present, which the
registry-driven reachability test fails on. Add the narrowest prefix that covers
your entry, and if you are widening one that carries a note, beat that note's
measurement in the same commit.

### Why the ratchet constants are L0's alone

Three lanes each subtracting their own delta from
`BASELINE_SYNTHETIC_STUBS_MANAGEMENT` produces a number that matches no tree.
The ratchet is `<=`, so a *decrease* passes silently and the constant drifts
above reality — that is exactly how three of them drifted by 3/3/12 before
2026-09-09. **The constant is read off the merged tree, never computed.** Report
`stubs before -> after` from your own run; L0 re-measures after merge.

## 5. Build queue — the binding constraint on parallelism

A release build is **17-50 minutes** and the host is shared. Rules:

- **At most two concurrent release builds.** Take the token before starting:
  `mkdir scratchpad/build-token-$LANE` succeeds only if you may build.
- **Batch your candidates.** Most of a lane's work needs no build at all: the
  candidate funnel runs off a dump from *any* recent binary, probe authoring and
  HotSpot oracle capture need no cratonvm build, and a source-scanning gate can
  be scored at two revisions with one binary. Prepare a whole wave, build once.
- **Never take a timing measurement.** This host's noise floor is worse than any
  effect you would claim: the same binary read 146 s and then 334 s on
  `RMapGcStress` in one session. Correctness deltas only.
- **`RMapGcStress` needs `TIMEOUT=600`.** Under concurrent load it times out and
  the corpus silently reports 131 vectors instead of 132.
- Run A/B arms **concurrently, not sequentially** — ABBA on this host read 1.9x
  for a flag that costs nothing.

## 6. Merge protocol

1. `git log origin/dev --grep=<your subject>` **before every merge.** A sibling
   lane can land your fix while you are measuring; it has happened.
2. Merge `origin/dev` before the acceptance measurement, not after. A measurement
   on an unmerged tree is not a measurement of what you will land.
3. Re-price every lever after merging — a lane that lands first can make your
   candidate already-retired or already-broken.
4. Verify a merge with **the whole gate set**, not the quick subset.
5. **Do not land while the widest gate is still running.**
6. Nothing is pushed without the human asking for it. `git push origin HEAD:dev`
   is its own command, keyed on the gate result.

## 7. This lane's own scope: 104 shadows, all accounted for

`java/lang/Class` (61), `Module` (23), `ClassFrameInfo` (5),
`module/ModuleDescriptor$Version` (5), `ModuleLayer` (4), `Class$Atomic` (3),
`ClassValue` (2), `Class$ReflectionData` (1).

**The account closes exactly: 54 retired + 23 held + 27 undispatched = 104.**
The two reviewed `Intrinsic`s sit outside that sum -- adjudicating a kind
removes a triple from the `Bridge` population, so they are no longer shadows to
count.

**Instrument:** `apps/probes/L0ClassModuleSurface.java`, 129 rows over the
whole surface, measured three ways against HotSpot 25.0.3+9 -- unarmed, and
with every native declining. `ClassNameSweep` reached only 10 of the 104; this
probe reaches 77.

```text
unarmed   8 diff lines of 130      yielded  58 diff lines of 130
```

Read as an aggregate that says *retire nothing here*, and it would be a wrong
conclusion drawn from a true number. **Per row:**

| | rows | verdict |
|---|---|---|
| OK → OK | 96 | native right, bytecode right → **retire** |
| BAD → OK | 4 | native **wrong**, yielding fixes it → **retire** |
| OK → BAD | 29 | native right, yielding breaks it → **hold** |
| BAD → BAD | 0 | |

The four unarmed diffs are exactly the BAD → OK rows, so **every disagreement
this VM has with HotSpot on lane 0's surface is one that retirement repairs.**

### The four repairs, one of which is not cosmetic

```text
Module.addExports("jdk.internal.misc", unnamed) on java.base
  HotSpot  threw java.lang.IllegalCallerException
  native   PERMITTED
ModuleDescriptor.Version.parse("")
  HotSpot  IllegalArgumentException: Empty version string
  native   returned a Version      (validation skipped)
Version.compareTo x2
  native   NPE in JDK bytecode, "ts1 is null" -- parse() built a Version whose
           internal lists were never filled
```

The first is an access-control check the native does not perform: only a module
may widen its own exports, and this VM let an unnamed module widen
`java.base`'s. The rest are the JDK's argument-validation layer, which is the
surface a retirement usually buys.

### Landed: 54 triples in `RETIRED_SHADOW_L0_TRIPLES`

Prefixes added: `java/lang/Class` and `java/lang/Module`, the narrowest that
cover the table. Both also admit classes that are *not* L0's, because a prefix
is a string; the table decides, and `java/lang/ref/` stays out so
`a_prefix_alone_retires_nothing` keeps its guard. Ratchets re-frozen from the
gates' own output: stubs 1883→1939 / 1894→1950 / 1883→1939, and the 54-vs-56
gap is a units difference (two `Version` triples are registered at two
ordinals each), enumerated rather than assumed.

### Held: 23 triples, each with the row that held it

| triples | why |
|---|---|
| `Class.descriptorString` | NPE — `componentType` field is null |
| `Class.getModifiers` | wrong **flag bits** (`public synchronized` for `Object`; `static` lost on a nested interface) |
| `Class.getAnnotation`/`getAnnotations`/`getDeclaredAnnotation`/`getDeclaredAnnotations`/`getAnnotationsByType`/`getDeclaredAnnotationsByType`, `isAnnotationPresent` (7) | annotations come back **empty** (`[]`, `null`, `0`, `false`) |
| `Class.newInstance` | `cachedConstructor` is null |
| `Module.getLayer`, `isExported` ×2, `isOpen` ×2 | answer `false` where HotSpot is `true` |
| `ModuleLayer.boot`/`findModule`/`modules`/`configuration` | `boot()` yields null |
| `Class.getPackage`/`getResource`/`getResourceAsStream`, `Module.getResourceAsStream` | **`NoClassDefFoundError`** — blocked pending **L7** |

`the_l0_held_families_are_not_retired` pins all of them -- **after a
correction.** It enumerated 21 of the 23 for a while, and nothing was red,
because a missing hold is only a hole: the two absentees were
`getAnnotationsByType` and `getDeclaredAnnotationsByType`, measured `OK -> BAD`
on rows 75 and 76 (HotSpot `1`, yielded `0`) and then never typed into the
array. Five of their seven siblings were pinned, which is the worst case -- the
family looks guarded.

It surfaced by **closing the population by subtraction and checking the residue
is empty**: 77 dispatched - 54 retired = 23, the array held 21, and the two
survivors of `dispatched - retired - held` were exactly the pair. The prose
above said 23 and the code said 21 for the same reason the prose was right,
that `getAnnotation*` is six methods and the row now spells them out. Any lane
adding a table should run that subtraction rather than trusting a hand count of
its own bullet list.

**`Module.isOpen` is held on a judgement, not a measurement, and that is
stated in the table.** Its rows agree with HotSpot when yielded — but they
agree at `false`, which is also what a blanket yield returns for the whole
family, and its sibling `isExported` demonstrably breaks. *An agreement that
cannot be distinguished from the default answer is not evidence.* It needs a
receiver whose correct answer is `true`, which `java.base` does not offer an
unnamed module; until that fixture exists, held.

### Not retired for want of an instrument: 27

Precondition 4 is per-instrument, and these read `invocations == 0` even in the
probe written to reach them. **14 + 8 + 5:**

**Fourteen cannot be called from Java at all** -- `getClassLoader0`,
`getEnumConstantsShared`, `reflectionData`, `newReflectionData`, `setSigners`,
the three `Class$Atomic` CAS methods, `Class$ReflectionData.<init>` and the
five `ClassFrameInfo` accessors are package-private plumbing reached only from
inside `java.lang.Class` and the stack walker. No probe will ever move these;
they need a unit test against the registry, not a Java fixture.

**Eight are `Module.implAdd*`** -- `implAddExports` x2, `implAddExportsNoSync`
x2, `implAddExportsToAllUnnamed`, `implAddOpens` x2,
`implAddOpensToAllUnnamed`. Same story one layer out: the module system's own
bytecode calls them.

**Five are exercised and still do not dispatch,** which is the interesting
group and not an instrument gap at all:

```text
Class.forPrimitiveName      probe calls Class.forName("int")          line 212
Class.getComponentType      probe calls Object[].class.getComponentType()  194
Class.getProtectionDomain   probe reads its own ProtectionDomain      line 322
ClassValue.remove           probe calls cv.remove(Integer.class)      line 343
Version.compareTo           probe compares parsed versions      lines 462-467
```

`Version.compareTo` is the one with a proven mechanism: row 126's failure names
`ts1`, a local in `Version.compareTo`'s **own bytecode**, so the JDK's method
served the call and the registration was never consulted. The other four share
the signature -- called, counted zero -- and their mechanism is **not
established per row**, so they are recorded here rather than explained. Five
registrations that a caller cannot reach are five candidates for deletion
outright, and that is a different question from retirement.

This is also why precondition 4 is taken per **triple** from
`--dump-native-registry` and never from "my probe calls this". A row that runs
green proves something about the JDK's bytecode, not about the native
underneath it.

### The two reviewed `Intrinsic`s

`Class.getModule` and `Class.getName` are **not** retirements and
`the_two_reviewed_intrinsics_are_not_retirements` pins that. Yielding gives a
null module and the internal name form (`java/lang/Object`) — worse than null,
because nothing throws. `getName` alone repaired 22 of 24 `ClassNameSweep`
rows, since the JDK derives `getTypeName`/`getCanonicalName`/`getSimpleName`
from it.

### The §1.4 reviewed-`Intrinsic` protocol, which every lane will need

Some shadows cannot be retired because **§1.4's remedy makes them worse**. The
contract's exception is a *reviewed* `Intrinsic`, and the review is a probe:

1. Establish the row is a genuine §1.4 shadow (bucket A or B; **not D**).
2. Establish that yielding is wrong, and say *how* — null, wrong value, or throw.
   "It breaks" is not a classification.
3. Write a sweep probe covering every receiver shape whose rules differ, and
   every accessor derived from the one you are tagging, so a fix to one that
   breaks another cannot hide. Print no identity hashes or anything the two VMs
   may choose independently.
4. Report all three numbers: **unarmed, yielded, and tagged**. A tag justified
   only by "yielded is broken" freezes whatever this VM happens to return.
5. `register` → `register_with_kind(..., NativeKind::Intrinsic)`, with the
   measurement in the rationale, and amend the kind-map row `bridge` →
   `intrinsic`, `kind_stated 0` → `1`.
6. **`registrar_drift` should now stay green. If it does not, read the bodies
   before touching the baseline.**

Until 2026-09-10 this step was a trap, and it is worth knowing why because the
same species will recur. `registrar_drift.rs` matched a registration by
requiring the byte after `register` to be `(`, so **`register_with_kind(..)`
was invisible to it** — 757 sites tree-wide, and not a uniform sample of the
registry: exactly the sites whose kind had been adjudicated. Since
`register` → `register_with_kind` is *this protocol's own remedy*, every
adjudication silently deleted the shipping half of whatever drift pair the
triple was in, and the gate reported the deletion as

```text
STALE BASELINE — recorded drift pair(s) no longer drift.
```

Good news wearing a defect's clothes, once per adjudication. The scanner now
accepts `_with_kind`, which surfaced **54 real pairs** that the blind spot had
been covering (see §8 — they are routed, not adjudicated).

So if this gate reddens after your tag:

- **Read both registrations' bodies first.** Same function ⇒ genuinely not
  drift, and `FIXED_NOT_DRIFTING` is right — with both-modes
  `--dump-native-registry` evidence, where `overwrote = null` is the
  load-bearing field. Different closures ⇒ the drift is real and the scanner
  has gone blind again; fix the scanner, not the baseline. Getting this
  backwards is how a false "fixed" assertion gets landed — it nearly was here.
- `DRIFT_TRIPLES` is the **allowed known-drift baseline**, not a defect list.
  `SSLContext.getProvider` was *added* to it. So an adjudicated triple usually
  belongs exactly where it already was and needs no baseline edit.
- Any re-take comes from the gate's own paste-ready block
  (`-- --nocapture`), never from arithmetic — and
  `registrar_reachability.rs`'s `FAMILY_DRIFT_EXPOSURE` must be re-taken in the
  **same commit**, which its own panic prescribes. It cross-checks per family
  and caught the phantom independently.
- Say in the record which rows moved and why. Both gates put it the same way:
  *"a re-take with no explanation is how a ratchet becomes a rubber stamp."*

**And run the gate set as `--tests`, never by naming targets.**
`cargo test -p <crate> --tests` stops at the first failing target, so a red
`registrar_drift` hid a red `registrar_reachability` behind it — and naming two
`--test` targets by hand hid both, which is how the `getModule` adjudication
shipped with this gate already red.

## 8. The 54 newly-visible drift pairs, routed

Widening the scanner surfaced 54 `(pass, triple)` pairs where one triple has
two implementations, one per compatibility mode: the synthetic-only body wins
under `--features synthetic-jdk`, and the shipping body is the only one in
every mode that ships, `--jdk-only` included. **They are recorded in
`DRIFT_TRIPLES`, not adjudicated** — each needs its own answer to "do the two
bodies agree?", and several are not cosmetic:

| triple(s) | owning lane |
|---|---|
| `ClassLoader.defineClass0` / `1` / `2` | **L7** |
| `Class.getSuperclass`, `isInstance`, `isAssignableFrom`, `isHidden`, `getPrimitiveClass`, `desiredAssertionStatus0`, `registerNatives` | **L0** |
| `ObjectStreamClass.hasStaticInitializer`, `initNative` | **L4** |
| `Unsafe.defineClass0` | **L5** |
| the remainder | by the ownership table in §2 |

The families whose exposure counts rose are `register_classloader_natives`
(81→84), `register_enterprise_final_natives` (118→135),
`register_java_lang_extras_natives` (27→28), `register_phase69_natives`
(8→11), `register_serialization_natives` (2→4) and
`register_unsafe_define_class` (1→2).

A lane adjudicating one of these does **not** need a release build: the
question is whether two bodies agree, which is a source question plus a
`--dump-native-registry` in both modes.

## 9. What "done" looks like

Per lane: its table covers every bucket-A/B row in its prefix set that passed
the four preconditions; every row it could not retire is classified in its page
as D (correct as-is), C/E/F (not a shadow), reviewed `Intrinsic` (with the
probe), or **blocked with the blocker named**. The last category is a result,
not a failure — `jdk/internal/loader/BuiltinClassLoader` failing to link blocks
ten vectors and no field publish reaches it.

Campaign-level: L0 reports one number per wave — bucket A+B eligible rows
remaining, from a fresh dump — and the corpus arm count beside it. The two move
independently and neither corrects the other: the 2026-09-09 wave cleared
**eleven** vectors' first failure and moved the total by **one**, because ten of
the eleven walked into the same structural blocker. A first-failure count cannot
score a fix in a chain.
