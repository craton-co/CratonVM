# `ObjectCleanerTest`: a live lambda is reclaimed by the young sweep — deterministic under G1 and Generational, never under ZGC

## Status

**OPEN, and no longer intermittent.** The 2026-09-04 revision of this page
recorded the symptom as "observed once, in a full 733-class run under 9-way
contention; not reproduced in 16 follow-up attempts", and hypothesised a
conservative-scan false positive (Family-A / G30). Both halves of that were
wrong:

* it is not rare — it reproduces **5/5 under `-XX:+UseGenerationalGC` and 5/5
  under `-XX:+UseG1GC`** on a quiet Windows box, in about three seconds a run;
* the 16 clean follow-ups were all on the **default collector**, which is ZGC —
  **0/5**. That is the entire reason it read as unreproducible. Same lesson as
  `a-per-collector-sweep-finds-bugs-the-default-cannot-reach`.

It is **not** a JIT defect (`--nojit` reproduces 4/5) and it is **pre-existing
on `dev`** (4/4 on a `dev`-tip binary built by another session).

## Reproducing

```bash
cd apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_NOCODE=1 CRATONVM_DBG_SWEEP_ZERO=1 \
  <cratonvm> --java-home <jdk25> --Xmx 1g -XX:+UseGenerationalGC \
  @common.args -Dcraton.batch=1 CratonRunner io.netty.util.internal.ObjectCleanerTest
```

`rc=1`, `ok=1 failed=2`, and the two diagnostics above print the whole story
with no further work.

| arm | non-clean runs |
|---|---|
| `-XX:+UseGenerationalGC` | **5/5** |
| `-XX:+UseG1GC` | **5/5** |
| `-XX:+UseG1GC --nojit` | **4/5** |
| default (ZGC) | 0/5 |
| `-XX:+UseZGC` | 0/5 |

Heap size is irrelevant: 2/3 at `--Xmx 256m`, 3/3 at each of 512m, 1g, 2g, 4g.

## What is actually happening

`CRATONVM_DBG_SWEEP_ZERO=1` names the victim at the moment of use:

```
[sweep-zero] RECLAIMED-LIVE receiver ptr=0x2430055bff0:
    original class=class_id=2147483768 (kind=0x00),
    zeroed by non-moving sweep cycle 0; invoked as java/util/Comparator.compare
[sweep-zero]     at java/util/Collections$ReverseComparator2.compare
[sweep-zero]     at org/junit/platform/engine/support/store/NamespacedHierarchicalStore.close
[sweep-zero]     at org/junit/jupiter/engine/descriptor/AbstractExtensionContext.close
[sweep-zero]     at org/junit/jupiter/engine/descriptor/TestMethodTestDescriptor.cleanUp
```

* `class_id=2147483768` is `0x80000078`; the high bit marks a **lambda proxy**
  class.
* The holder is `java.util.Collections$ReverseComparator2`, whose only field is
  `final Comparator<T> cmp` and whose `compare` is `return cmp.compare(t2, t1)`.
* That object is JUnit's
  `NamespacedHierarchicalStore$EvaluatedValue.REVERSE_INSERT_ORDER`, a
  `private static final Comparator` built in `<clinit>` as
  `Comparator.comparing(EvaluatedValue::getOrder).reversed()` — confirmed with
  `javap -p -c` on `junit-platform-engine-1.14.3.jar`.

So **the holder survives, its `cmp` field survives, and the lambda that field
points at has been freed and zeroed.** The next `compare` dispatches on an
all-zero header, `class_id` reads 0, and the interpreter resolves the abstract
interface declaration:

```
java.lang.AbstractMethodError: method java/util/Comparator.compare(Ljava/lang/Object;Ljava/lang/Object;)I has no Code attribute
	at java.util.Collections$ReverseComparator2.compare(Collections.java:5756)
	at org.junit.platform.engine.support.store.NamespacedHierarchicalStore.close(...:136)
```

That is exactly the message the earlier revision reported. It was right that no
`Comparator` appears in the test's own source — the comparator belongs to
**JUnit's extension-context store**, which this test reaches only because
`@Timeout(5000)` puts a value in that store for each of its three methods.

**The same run produces two more faces of the one defect**, which is why the
class reports `found=3 started=2` — the engine dies mid-class:

```
[DBG_NOCODE] MutableExtensionRegistry$Entry.getExtension()Ljava/util/Optional; has no Code attribute
             | recv_cid=0 recv_class=java/lang/Object recv_kind=Object recv_is_declaring=false
NoSuchMethodError  method="java/lang/Object.getTestInstanceLifecycle()Ljava/util/Optional;"
[DBG_NOCODE] java/util/function/Predicate.test(Ljava/lang/Object;)Z has no Code attribute
```

`recv_cid=0` with `recv_kind=Object` is the signature: a heap cell whose class
stamp is gone — not an array, not an unstamped synthetic. The interpreter's own
tripwire agrees: `Stale pointer detected in invokevirtual receiver (…, all-zero
header)`.

## The collector's own verdict

`CRATONVM_DBG_SWEEP_EDGES=1` classifies the cycle, and the answer is stable:

```
[sweep-edges] SUMMARY marked=22458 unmarked=13492
              | edges: root=11 young-survivor=0 old-gen=0
              => REACHABLE NODE WILL BE SWEPT — case (b) marking/seeding bug
```

`root=11` appears in **10 of 10** failing runs and `root=0` in the single run
that passed under that flag — the tightest correlation measured here. Each of
the 11 is a root the marker was handed and did not retain:

```
[sweep-edges] (1) ROOT @0x2675361dc08 -> UNMARKED young obj
              (class_id=1745 kind=0x00 num_slots=3 array_len=0)
              — mark filter rejected a live root?
```

The corruption is confined to the **first** young collection: `cycle=0` carries
every symptom, `cycle=1` is clean.

**Do not over-read `root=11`.** The `CRATONVM_GC_LATE_RESOLVE_DROPPED` arm below
shows those candidates resolve to no object base at all, so they are ordinary
conservative false positives pointing at gap space — which the collector's own
contract says can only over-retain. The `(1) ROOT -> UNMARKED` line resolves a
root through a PARTIAL oracle and calls anything it cannot cover a live root, so
"case (b) marking/seeding bug" is the diagnostic's inference, not a measurement.
The correlation with failure (10/10 vs the one passing run's 0) is real and
unexplained, but it is not yet evidence that these eleven are the victims.

## Hypotheses tested and REFUTED

Each was a plausible mechanism; each is now excluded by measurement, so the next
session need not re-run them.

| hypothesis | how it was tested | result |
|---|---|---|
| conservative-scan false positive (this page's original guess) | `recv_kind` / `recv_cid` from `CRATONVM_DBG_NOCODE` | **refuted** — `recv_cid=0`, not a plausible-looking fake header |
| a JIT defect | `--nojit` | **refuted** — 4/5 |
| young **relocation** (the `DoHead` Generational family, `CRATONVM_NO_MOVING_YOUNG=1`) | 6-rep arms, both collectors | **refuted** — 5/6 and 6/6 with the lever on |
| an object-grid **desync** (`arena already corrupt before this sweep`) | 10 runs, desync vs failure | **refuted** — desync in 1 of 10 failing runs |
| an old→young **card / remembered-set** miss | `CRATONVM_DBG_RSET_AUDIT=1`, which walks every old object and checks every old→young edge against the card bitmap | **not applicable, and read the zero carefully** — the audit prints only when it finds edges, and it printed nothing, i.e. `edges == 0`. That is a VACUOUS "no misses": there were no old→young edges at all in this collection. What it does establish is that the holder is not an old-gen object, because at cycle 0 old gen holds no reference into young |
| a **live heap object** still pointing at the victim | whole-heap inverted holder scan (below) | **refuted** — 0 references, young and old |
| a **root** pointing into a freed span | the unconditional `ROOT_IN_DEAD_SPANS` guard, and `CRATONVM_DBG_SWEEP_LIVENESS=1` | **refuted** — neither ever fires |
| a CratonVM **native** holding the lambda in a Rust local across a GC (the Family-1 stale-`ObjectRef` shape) | `--dump-native-registry` | **refuted** — `java/util/Comparator` has **no** registrations in this build, and `Collections.reverseOrder` none either; the whole path is real bytecode |
| the anchor oracle **dropping a live root** as free/gap space inside a "proved" span (`mark_young`'s `oracle_dropped` arm — the one decline with no second chance, unlike `oracle_unresolved`) | a new default-off arm, `CRATONVM_GC_LATE_RESOLVE_DROPPED=1`, that runs the dropped candidates through the SAME grid resolver the unresolved ones get and marks whatever resolves | **refuted** — `LATE_RESOLVE_DROPPED_RECOVERED` stays 0 and the arm is 5/5 bad, i.e. every dropped candidate really is gap space and the oracle's proofs hold. The arm is kept: it is over-retention-only, it costs nothing when the proofs are right, and it is the cheapest way for the next session to re-ask this question |
| any single G1 / Generational tuning lever | 14-arm sweep: `PARALLEL_EVAC`, `PARALLEL_MARK`, `FULL_RSET_SCAN`, `SCRUB_FREE`, `INLINE_BARRIER`, `NARROW_FIXUP`, `EDEN_STRIPES`, `SHARED_ALLOC`, `WORKERS=1`, `LATE_HEADER_WRITE`, `PAR_THREADS=1`, `NO_LIVE_REGION_MEMO`, `PRECISE_ONLY_ROOTS`, `SYNC_YOUNG_WIPE` | **refuted** — none takes the arm to 0 |

Minimal Java reproductions of the *shape* also do **not** reproduce, and that is
itself a datum — the trigger needs more than this object graph. All clean on
both affected collectors:

* a `private static final Comparator.comparing(fn).reversed()` used across
  `System.gc()` — 200 rounds;
* the same driven by allocation-forced young collections instead of
  `System.gc()` — 40 rounds;
* the same used from four `ForkJoinPool` workers while another thread forces GC
  and short-lived threads die;
* the same with the `<clinit>` forced onto a worker thread and made to allocate
  ~48 MB *between* `comparing(...)` and `reverseOrder(...)`, so a young
  collection lands while the lambda is only on that frame's operand stack;
* netty's own `ObjectCleanerTest.testCleanup` re-implemented in the default
  package, with and without `@Timeout`.

## What remains, and the instruments for it

Everything the collector can be asked from outside says the victim was
unreachable at mark time: no heap holder (young walked as objects, old walked as
objects), no root, no old→young edge, no native local, and no dropped-candidate
proof that turns out to be wrong. The heap-side space is exhausted.

What is left is the one root source none of these instruments can see the inside
of: a **Java frame** — an operand-stack or local slot on some thread — holding
the lambda between `Comparator.comparing(fn)` producing it and
`Collections.reverseOrder(cmp)` storing it. The victim is reclaimed in the FIRST
young collection, which is when JUnit is still loading and initialising classes,
so `<clinit>` frames are exactly what is on the stacks. Four probes that
reproduce that shape deliberately (including one that forces the `<clinit>` onto
a worker thread and allocates 48 MB inside it, between the two halves) are all
clean, so it is not the shape alone — something about the real frame, thread, or
class-init state matters.

The next instrument should therefore be on the ROOT-GATHERING side rather than
the sweep side: for the failing collection, dump every thread the STW root scan
enumerated and every frame it scanned, and check that count against the thread
list the VM believes it has. `CRATONVM_DBG_MTROOTS` publishes the GC's
initiating thread and its blocked-thread count and is the obvious place to
start; note that the `[sweep-zero]` record for this defect prints
`gc reason=unknown initiator_tid=0 blocked_threads=0`, i.e. the collection did
NOT come through the path that publishes that context, which is itself worth
chasing.

Two diagnostics were sharpened while chasing this and are now permanent (both
opt-in, both in `gc/src/gen_heap.rs`):

* **`CRATONVM_DBG_SWEEP_CENSUS=1` now runs an INVERTED holder scan.** The
  pre-existing per-victim scan is `O(victims x heap)` and so is bounded to six
  victims — on a 20,000-object cycle that is a 0.03% sample, and the wrong one
  (the six lowest addresses). The new pass builds the dead spans once, walks
  young from-space **as objects** and old gen once, and reports only victims
  that DO have a holder, naming the holder's class and slot offset, with a
  per-cycle total where `0` is the only clean answer. Walking objects rather
  than raw words is load-bearing: a flat word scan over the same arena reported
  **240 phantom "live holders"** that were words in free/gap space; the object
  walk reports 0.
* **`CRATONVM_DBG_SWEEP_EDGES=1`'s desync report now names the culprit.** A walk
  desyncs at the object AFTER the one whose recorded size was wrong, so the
  offset it printed was always the victim's, never the culprit's. It now prints
  the offending header's `class_id`/kind/`num_slots`/`array_length`/
  `element_type`/`gc_flags` and a hex window, plus the PREVIOUS object's
  identity and size.

## Not to be confused with

The `AbstractMethodError` text is shared with the `Comparator$Native` cluster in
`native-collections` (see the block comment above its registrations, which
records the Groovy/ANTLR4 `ParserATNSimulator.STATE_ALT_SORT_COMPARATOR` case).
That cluster is **not** registered in this build — `--dump-native-registry`
shows zero `java/util/Comparator` rows — so the two are unrelated here despite
the identical message.
