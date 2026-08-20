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

**Not investigated.** Stated as the first question the table raises, not as a
conclusion.

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

* **N1 — diagnose `RMapGcStress` under one armed prefix.** It is the cheapest
  question on this page and it re-prices four rows of the table.
* **N2 — arm `HashSet` and `Hashtable` together** and see whether the failures
  compose or interact. Two prefixes, one command, and it is the first real test
  of whether this migration can proceed family-by-family at all.
* **N3 — put this matrix in CI as a scheduled non-blocking job.** It is six env
  vars over an existing run and it is the only instrument in the tree that
  prices a retirement before it is attempted. Today it exists because somebody
  remembered to type it.
* **N4 — re-derive the row's "order: `native-awt` first, `native-collections`
  last and subdivided by collection family"** against §3. The subdivision is
  right; the ordering within it was never measured.
