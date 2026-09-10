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
| **L0** (this page) | `java/lang/Class*`, `java/lang/Module*`, `java/lang/module/` | 131 | 9 | 131 |
| **LT** | *whole cross-cutting registrars* (see §3) | **1,100** | 87 | **57** |
| **L1** | `java/util/` (less `concurrent/`), `java/text/`, `sun/util/`, `java/time/` | 963 | 98 | 726 |
| **L2** | `java/lang/` remainder, `java/math/` | 434 | 68 | 303 |
| **L3** | `java/lang/reflect/`, `jdk/internal/reflect/`, `sun/reflect/`, `java/lang/invoke/` | 251 | 36 | 246 |
| **L4** | `java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign` | 1,110 | 131 | 615 |
| **L5** | `java/util/concurrent/`, `jdk/internal/misc/`, `sun/misc/`, `java/lang/Thread*`, `jdk/internal/vm/` | 405 | 23 | 345 |
| **L6** | `java/net/`, `sun/net/`, `javax/net/`, `java/security/`, `sun/security/`, `javax/crypto/`, `javax/security/`, `jdk/net/` | 819 | 90 | 663 |
| **L7** | `java/lang/ClassLoader*`, `jdk/internal/loader/`, **`java/security/SecureClassLoader`** (claimed 2026-09-10, one `<clinit>` row, from L6) **+ the bootstrap failure triage** | 20 | 6 | 18 |
| — | **UNOWNED, frozen** | 316 | 83 | 291 |
| | **TOTAL** | **5,549** | **631** | **3,395** |

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
| same file — `RETIRED_SHADOW_PREFIXES` | **L0** | pre-populated; never edit |
| same file — the N-way disjointness test | **L0** | never edit; it must fail if you collide |
| `native-builtins/tests/stub_ratchet.rs` — the three `BASELINE_*` constants | **L0** | **never edit.** Report your measured delta in your commit message |
| `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — **rows** | the lane | keyed class+name+descriptor, disjoint by construction |
| same file — the header note | **L0** | never edit |
| `regression-suite/bridge-ratchet.sh` | **L0** | never edit |
| [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) | **L0** | propose via your lane page |
| `apps/probes/L<N>*.java` | the lane | namespace your probes; `apps/` is gitignored, so `git add -f` |

### The skeleton commit makes the chain conflict-free

L0 lands this **before any lane starts**:

1. Nine empty tables, `RETIRED_SHADOW_L0_TRIPLES` … `RETIRED_SHADOW_LT_TRIPLES`.
2. Nine `||` arms in `triple_is_retired_shadow`, one per table, in a fixed order.
3. Every lane's prefixes pre-added to `RETIRED_SHADOW_PREFIXES`.
4. An N-way disjointness test over all tables, and a sortedness test per table.

Adding a prefix while its table is empty is **provably inert**: the prefix list
is only an early-out for the binary search, so a wider list plus an empty table
answers `false` for exactly the same inputs. That is what makes it safe to land
all nine prefixes up front — and it is why a lane never has to touch the
prefixes or the chain, so two lanes never conflict textually in this file.

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

## 7. This lane's own retirement scope

131 shadows over 9 classes: `java/lang/Class` (61), `java/lang/ClassLoader`
(27 — **L7 owns the loader story; L0 owns only rows whose remedy is a `Class`
question**), `java/lang/Module` (23), `ClassNotFoundException` (15) and
`ClassCastException` (14) — *both of which belong to lane T's throwable
registrar, not here* — plus `ModuleLayer`, `ClassValue`, `Class$Atomic`.

Two are already resolved and are the pattern the other lanes should copy:

- **`Class.getModule` → reviewed `Intrinsic`.** Not `ACC_NATIVE`, so §1.4 makes
  it a shadow; but `Class.module` is written only by a real VM at class
  definition, so yielding returns **null**, which is 12 of the corpus failures.
  Reviewed with `apps/probes/ClassModuleSweep.java` (32 rows, 31 matching).
- **`Class.getName` → reviewed `Intrinsic`.** Yielding returns the **internal
  form** (`java/lang/Object`), which is worse than null because nothing throws;
  it propagates into every JDK name comparison and is why `ServiceLoader`
  reports *"module java.base does not declare `uses`"*.

  **This bullet described a tag that had not landed, and said so in the past
  tense for a day.** Measured 2026-09-10: the registration was still
  `bridge`/`kind_stated:false` in `--dump-native-registry`, the kind-map row
  still read `bridge 0 1`, and `apps/probes/ClassNameSweep.java` existed in no
  commit — `git log --all -S ClassNameSweep` finds only the commit that wrote
  this bullet. It landed that day, reviewed with a sweep of **85** rows (not
  24): **0 rows differ unarmed, 9 differ armed before the tag and 1 after**, and
  the nine include `Class.forName(X.class.getName())` throwing
  `ClassNotFoundException` for every reference type. One tag repairs eight of
  them, because the JDK derives
  `getTypeName`/`getCanonicalName`/`getSimpleName`/`toString` from `getName`.
  The ninth is a `Class.forName` defect on a NESTED application class and is
  recorded, not frozen — the sweep is checked in.

  Two things this cost, worth keeping: a prose claim of "already resolved" is
  not a measurement, and the armed/unarmed split alone would have missed it —
  rows reached through a method reference kept the native and answered
  correctly, so a probe that asked one dispatch route called the family clean.

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

An `Intrinsic` is **exempt at every dispatch door and exempt from the census by
construction**. That is a real cost: it removes the row from the population the
dial can ever ask about. Earn it with numbers or leave it a `Bridge`.

## 8. What "done" looks like

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
