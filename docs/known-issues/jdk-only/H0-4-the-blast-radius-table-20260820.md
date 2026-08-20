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
