# H0-4 — the blast-radius table, and the migration order it dictates

**Status: OPEN — MEASURED.** Six runs of the full 104-vector strict arm, one
armed prefix each, on `C:/craton/target-jdkonly-h2/release/cratonvm.exe` at
`db71dfb40`, whose **unarmed baseline in the same session is 104 / 104**. No
source change.

Lane H0 (orchestrator), 2026-08-20. Extends `H0-3`, which measured the first
row and asked for the other five.

---

## 1. The table

`CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` makes contract §1.4 **enforced**
rather than counted for that prefix: the native stops winning and real JDK
bytecode runs. That is the same thing a permanent retirement does, so this
prices a retirement **before** anyone attempts it, for one env var and no build.

| prefix | passed / 104 | failed | vectors |
|---|---:|---:|---|
| `java/util/HashSet` | **103** | **1** | `RMapGcStress` |
| `java/util/Hashtable` | **101** | **3** | `RMapGcStress` `RJdkBridge1` `RJdkEnumerations` |
| `java/util/LinkedHashMap` | 97 | 7 | `RMapGcStress` `ROverlaySystemGcStress` `RForeignLayoutCollections` `RJdkServices` `RJdkJmx` `RJdkMapViews` `RServiceLoaderDoubleSource` |
| `java/util/TreeMap` | 97 | 7 | `RCollections` `RTreeRangeGc` `RJdkViews` `RJdkBridge1` `RJdkCollections` `RJdkJmx` `RJdkEnumerations` |
| `java/util/concurrent/ConcurrentHashMap` | 93 | 11 | see `H0-3` §2 |
| `java/util/HashMap` | **81** | **23** | `RCollections` `RSerial` `RExecutorShutdown` `RBlockingQueue` `RChannelInterrupt` `RMapGcStress` `RFileTimes` `RJdkViews` `RSimpleTimeZoneRaw` `RJdkOptionalShape` `RJdkStrict` `RJdkCollections` `RJdkModule` `RJdkExecutors` `RJdkForkJoin` `RJdkNio` `RJdkProcess` `RJdkSecurity` `RJdkJmx` `RJdkFailure` `RJdkLogging` `RJdkEnvMap` `RJdkEnumerations` |

### ⚠ READ THIS BEFORE USING THE TABLE (added 2026-08-20, `H0-7` §4)

**This table orders FAMILIES by aggregate cost. It does not order what any
individual vector suffers, and it must not be read as a per-vector priority.**

`H0-7` measured the inversion directly. In aggregate `HashMap` (81) is far more
expensive than `ConcurrentHashMap` (93). But for `RJdkLogging`, arming
`HashMap` leaves it running to completion with **76 of 79 checks correct**,
while arming `ConcurrentHashMap` **kills it before it prints anything**. For
that vector the cheap family is the fatal one.

Stated here rather than only in `H0-7` because this table is the most-quoted
artefact of the wave and three lanes have planned against it.

## 2. What this settles

**The collection families are not equally entangled, and nobody knew that.**
The spread is 1 to 23 — a factor of twenty-three — and every previous argument
about "the collection cluster" has treated it as one undifferentiated body.
`G88-1` §5 retagged eight registrars together and read the resulting breakage as
evidence that *"VM-owned state is pervasive"*; §5's own correction narrowed that
to "a partial retag fails". **Both readings are coarser than the tree.** Some of
this cluster is nearly free to move and some of it is the floor.

It also corrects `H0-3`'s implied conclusion. That record measured CHM at 11 and
called it *"the floor the VM stands on"*. **`HashMap` is the floor**; CHM is one
storey up. The eight non-collection subsystems `H0-3` found under CHM —
crypto, security, logging, modules, proxies, service loading — appear again
under `HashMap`, alongside executors, ForkJoin, NIO, process, JMX, serialization
and the environment map. `H0-3` §3's finding is right in kind and understated in
degree.

## 3. The migration order, which is the point of the table

Cheapest first, and each step is independently verifiable because the dial can
be armed on one prefix at a time:

1. **`java/util/HashSet` — 1 vector.** The obvious first migration. One vector
   to diagnose, and `RMapGcStress` fails for four of the six families, so it is
   probably not even `HashSet`-specific (§4).
2. **`java/util/Hashtable` — 3.** Two beyond the common factor.
3. **`LinkedHashMap` / `TreeMap` — 7 each**, and *different* sevens: only
   `RMapGcStress` and `RJdkJmx` overlap. They are separable work, not one job.
4. **`ConcurrentHashMap` — 11.**
5. **`java/util/HashMap` — 23, last.** Nothing above it can move until it does.

**This inverts the P0 row's advice.** *Wholesale `Bridge` over-tagging* says to
do `native-collections` "last and subdivided by collection family". Subdivided,
yes — and the subdivision now has a measured order instead of an alphabetical
one. But "last" is wrong for `HashSet` and `Hashtable`, which are the cheapest
things in the entire cluster and are currently queued behind the most expensive.

## 4. `RMapGcStress` is a common factor and probably not about collections

It fails under **four** of the six prefixes (`HashSet`, `Hashtable`,
`LinkedHashMap`, `HashMap`) and it is the *only* failure for `HashSet`. Before
anyone diagnoses six separate defects, diagnose that one: if it is a single
GC-interaction defect that every armed prefix exposes, the real per-family costs
are 0, 2, 6, 7, 10 and 22, and the first two families are essentially free.

### ANSWERED, same day, measured — one defect, four faces

`RMapGcStress` run alone under each prefix in turn. Every failure is the same
mechanism, and each one names it:

| armed prefix | the assertion |
|---|---|
| `java/util/HashSet` | `NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because the return value of "java.util.Iterator.next()" is null` |
| `java/util/Hashtable` | `Hashtable/filled: lost/wrong value for 1 -> null` |
| `java/util/LinkedHashMap` | `LinkedHashMap/filled: iterated 1 != 3000` |
| `java/util/HashMap` | `HashMap/filled: iterated 1 != 3000` |

**Real bytecode iterating a table the VM never populated.** `iterated 1 != 3000`
is the sharpest of the four: the container reports one entry where three
thousand were inserted, because the inserts went to a CratonVM side structure
and the iteration reads the real `table`. That is `H4-1` §1's mechanism — *"a
silently empty map, no error"* — caught in the act, and it is one defect
presenting under four different family names rather than four defects.

**So the per-family costs in §1 are inflated by one shared row.** Net of
`RMapGcStress`, the table reads **0 / 2 / 6 / 7 / 10 / 22**, and `java/util/HashSet`
has **no remaining objection from this corpus at all**.

**What that does NOT license.** §5's first bullet still stands and now matters
more: a zero here means "these 104 vectors raise no objection", not "`HashSet`
is retirable". The corpus has no AWT vector at all (`G79-1`), and `G90-1` §5 is
the standing instance of a screen passing what the wider arm rejected. A zero is
permission to attempt the migration and measure it — not permission to skip
measuring it.

## 5. What this does NOT establish

* **A green cell is not a clean family.** These are 104 vectors, and `H0-3`'s
  own lesson is that a corpus reports a pass on subsystems it does not ask
  about. `HashSet` at 103/104 means "one vector in this corpus objects", not
  "`HashSet` is retirable". The corpus has no AWT vector at all (`G79-1`), for
  scale.
* **It does not identify a single mechanism.** Each cell is a vector count, not
  a diagnosis. `H0-3` §5 N2 — read the failures and ask whether they all reduce
  to "real bytecode read an empty table" — is still open, and the answer
  determines whether this is one defect with 23 faces or several.
* **It says nothing about compatible mode.** The dial only affects §1.4
  enforcement under `--jdk-only`.
* **The prefixes are not disjoint in effect.** Arming `HashMap` may already
  enforce shadows on classes the `LinkedHashMap` run also touches, since
  `LinkedHashMap extends HashMap`. The per-family numbers are each measured
  alone; **their sum is not the cost of arming all six**, and nobody has run
  that combination.

## 6. NOMINATIONS

* **N1 — DONE, see §4.** One defect with four faces; the table re-prices to
  0 / 2 / 6 / 7 / 10 / 22.
* **N2 — arm `HashSet` and `Hashtable` together** and see whether the failures
  compose or interact. Two prefixes, one command, and it is the first real test
  of whether this migration can proceed family-by-family at all.
* **N3 — DONE (lane `H10`, 2026-08-20).** `scripts/jdk-only-blast-radius.sh`
  plus `.github/workflows/jdk-only-blast-radius.yml`, weekly and
  workflow-dispatch, `continue-on-error`. **Two deviations from what I asked
  for, both improvements:** the adjudicated cell is the **failing SET, not the
  pass count** — a count moves whenever the corpus grows, and it grew three
  times in two days, so a count-based cell would have cried regression on every
  new vector; and it runs an **unarmed control arm first** and nets every cell
  against it, generalising §4's `RMapGcStress` subtraction instead of
  hard-coding it. It also refuses to print a total, on the grounds that the
  cells do not sum and the prefixes are not disjoint (`LinkedHashMap extends
  HashMap`) — which is the objection §5 raises against my own table.
  **The shipped baseline is explicitly non-adjudicating** (transcribed, other
  binary, other OS, 104-vector corpus) and the script exits 2 rather than
  scoring against it, so **the first adjudicating baseline is still owed** —
  see `H10-1` N4.
* **N4 — re-derive the row's "order: `native-awt` first, `native-collections`
  last and subdivided by collection family"** against §3. The subdivision is
  right; the ordering within it was never measured.

---

## 7. CORRECTION to §4 (lane H0, 2026-08-20) — the table is populated; ITERATION is what breaks

§4 concluded that `RMapGcStress` is *"real bytecode iterating a table the VM
never populated — 3000 inserts went to a side structure"*. **That is wrong, and
lane `H13` caught it.** I re-measured rather than take the correction on trust,
and `H13` is right.

Probe saved as `regression-suite/probes/HashMapArmedStressProbe.java`: 3000
`String`-keyed puts into a `HashMap`, then `size()`, then 3000 `get()`s, then
both iterations. Armed `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap`,
`--jdk-only`:

| | HotSpot | CratonVM armed |
|---|---:|---|
| `size()` | 3000 | **3000** |
| 3000 × `get()` returning the right value | 3000 | **3000** |
| `entrySet()` iteration | 3000 | **`ClassCastException` after 0** |
| `keySet()` iteration | 3000 | **1** |

**The inserts did not go anywhere else.** The table holds all 3000 and real
bytecode reads every one of them back correctly. What fails is walking it.

**Why the wrong conclusion was tempting, and what it cost.** §4 reasoned from a
vector's symptom — `iterated 1 != 3000` — to a cause, without running the two
cheap discriminators (`size()` and `get()`) that separate "never stored" from
"stored but unwalkable". Those two commands would have taken a minute. The
false cause then propagated: it is the sentence `HANDOFF-20260820` §6b item 2
quotes, and it argued for the wrong repair — populating a table that was never
empty.

**The right statement, which converges with `H0-6`:** the nodes are stored, and
they are the wrong *class*. `H0-6` §7 measured that `AnonymousObject$4` **is**
the `HashMap.Node`. So `entrySet()` throws `ClassCastException` because the node
cannot be cast to `Map.Entry`, and `keySet()` yields 1 because the walk over
those nodes cannot follow `next`. **One root — the node's class identity — with
`size`/`get` unaffected because those paths never type the node.**

This also re-reads §4's own evidence: the "lost value" under `Hashtable` and the
`Iterator.next()` NPE under `HashSet` are iteration failures too, not storage
failures. The "one defect, four faces" conclusion **survives**; only its
mechanism was wrong.

---

## 8. SECOND CORRECTION (lane H14, 2026-08-21) — this table is not the cost ordering

Two findings from the first full census of the 1402 change what this table may
be used for. Both are `H14`'s; I am recording them here because **this table is
the most-quoted artefact of the wave** and a reader who stops at §1 gets a wrong
plan.

### 8a. `java/util/Properties` is 65/104 — WORSE than `HashMap`

`H14-3` armed thirteen registrars. `java/util/Properties` costs **39 vectors**
against `HashMap`'s 22, with `RJdkHello` among the failures. **`HashMap` is not
the floor and not the worst.** §3's "migration order, which is the point of the
table" is ordered on six prefixes chosen because they were the collection
families somebody had already named — not because they were the expensive ones.

Anyone sequencing from §1 is sequencing from a **sample of six**, and the sample
was not drawn to be representative.

### 8b. The six prefixes are 14.3% of the defect

`H14-2`, measured: the six families in §1 account for **200 of the 1402** rows,
and **135 of the 149 registrars have ZERO rows under any of them.** A further
**445 rows (31.7%)** are claimed by no P0/P1/P2 row at all — `java.lang` core
168, `java.io` streams 99, `StringBuilder`/`StringBuffer` 57,
`java.lang.invoke` 56.

**So this table prices a seventh of the problem, and the effort's whole
published queue is aimed at that seventh.** That is not a defect in the
measurement — every number in §1 is still correct for what it measured — but it
is a decisive limit on the conclusion, and §5's "what this does NOT establish"
did not anticipate it. It should have: the six prefixes were the ones already
under discussion, which is the definition of a convenience sample.

### 8c. And §4's `RMapGcStress` netting needs a boundary

`H14-3` found `RMapGcStress` failing in **12 of 13** of its arms with `rc=124` —
a **TIMEOUT**, not an assertion failure: the vector needs **233 s unarmed**
against a 120 s budget and passes armed at 600 s. **That is the clock, not a
defect**, and netting it out of a cost cell would silently subtract a vector
that was never objecting.

`H14-3` is explicit that this is **not** the same `RMapGcStress` finding §4
netted out — §4's were real assertion failures (`iterated 1 != 3000`, a lost
value, an NPE from `Iterator.next()`). Both are true, and they are different
runs of the same vector name. **When netting a shared row out of a cost table,
check the failure MODE and not only the vector name.** §4 stands; this is a
boundary on how its method may be reapplied.

### What survives

The 23× spread is real, the per-family numbers are real, and `RMapGcStress`-as-
one-defect-with-four-faces (§4) and the node-class mechanism (§7) both stand.
**What does not survive is reading §1 as "the migration order".** It is six
priced cells out of a 1402-row population whose real distribution `H14-2` now
carries.

---

## 9. THIRD CORRECTION (2026-08-21) — the instrument does not simulate a retirement

`H16-3` measured, and lane H0 independently reproduced, that
**`CRATONVM_ENFORCE_NATIVE_SHADOW` yields to real bytecode exactly ONCE per
process** — not per call site, not per receiver.

Three `HashMap`s, three puts each, one process, armed `java/util/HashMap`,
`HashMap.table` read reflectively:

| | `table` class | real `Node` | fabricated |
|---|---|---:|---:|
| HotSpot | `[Ljava.util.HashMap$Node;` | 3 | 0 |
| CratonVM unarmed | `[Ljava.lang.Object;` | 0 | 3 |
| **armed, map1** | **`[Ljava.util.HashMap$Node;`** | **1** | **2** |
| armed, map2 | `[Ljava.lang.Object;` | 0 | 3 |
| armed, map3 | `[Ljava.lang.Object;` | 0 | 1 |

**So §1's cells price a HYBRID state that no retirement can reach.** `map1` is a
real `Node[]` holding one real node and two fabrications — internally
inconsistent in a way neither the current VM nor a fully-retired VM produces.

**The direction of the error is not knowable from this**, and I am not claiming
the numbers are too high or too low: a hybrid table can be worse than
uniform-fabricated (a real `Node[]` rejects a store an `Object[]` accepts) or
better (one fewer fabrication). What is knowable is that **"arming costs N
vectors" is not the proposition "retiring costs N vectors"**, and this table has
been read as the second for two days.

**What survives, and it is not nothing.** A cell that reports a **failure** is
still reporting an observed wrong behaviour — `H22`'s `StringBuilder` and
`Throwable` refusals get *stronger*, not weaker. It is the clean **zeros** that
become unreliable, because a state that yields once may simply not have yielded
anywhere that mattered.

### This is the third correction to this record in two days

§7 corrected its mechanism, §8 corrected its coverage and its cost ordering, and
§9 corrects the instrument. **The measurements were all real; the conclusions
drawn from them were repeatedly wider than the measurement supported.** That is
worth saying plainly at the bottom of the most-cited page in this directory:
*this table earned its authority from being the first thing measured, not from
being the right thing measured.*
