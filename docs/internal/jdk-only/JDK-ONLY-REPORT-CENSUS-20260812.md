# `--jdk-only-report` is a complete census, and nothing was using it

**Measured 2026-08-12 on `cratonvm-merged-dev` (dev tip `210703b7a`), Windows.**

## The instrument

```
cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp <cp> <Main>
```

On one ordinary Java program this emitted **1569 violations in three kinds**:

| kind | rows | what it is |
|---|---:|---|
| `compatibility-class-requested` | **19** | a native asked the VM to fabricate a class |
| `native-shadows-bytecode` | **226** | a native running in front of real JDK bytecode |
| `synthetic-native-registered` | **1324** | synthetic natives registered |

Every row carries `class`, `requester` (`file:line`), `initiating_loader` and
`reason`. `schema_version: 1`, so it is meant to be consumed.

This matters because the roadmap treats the Phase 1 blocker set as something to
be discovered by reading tables and the Phase 2 target (its "3956 bridges") as
something to be derived from a registry dump. **The VM already reports both,
per-run, attributed to a source line.** The 226 `native-shadows-bytecode` rows
are Phase 2's target, measured on the path a real program actually took, rather
than counted statically.

## Correction: an error of mine, and how it happened

Earlier in this campaign I reported that the shutdown
`[jdk-only:shutdown] policy violation` report "names one requested class" and
appeared truncated. **That was wrong.** The run emits 1570 lines; I had piped
the output through `head -40` and read the first block as the whole report.

A lane was given that claim and correctly could not explain it from source —
its reading said all 13 should print, and its reading was right. It filed the
discrepancy as a residual with a decisive check attached. That check is what
resolved it, against me.

Two things worth keeping:

* **The same mistake, a third time.** This campaign has now twice reported a
  pipeline's own behaviour as the subject's: a successful 25-minute build read
  as "failed" because the command ended in `grep -c`, and a census claim whose
  `exit=0` was grep's status. This one is the same family — `head` truncating a
  report and the truncation being read as the VM's.
* **A subordinate lane's "I cannot derive your claim from the source" was the
  correct signal, and it was correct.** It is worth treating that response as
  evidence rather than as the lane failing to find something.

## The 19 fabrication requests, complete, with requesters

```
cratonvm/internal/UnmodifiableCollection    <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableEntryItr      <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableEntrySet      <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableItr           <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableList          <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableListItr       <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableMap           <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableMapEntry      <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableNavigableSet  <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableSet           <- vm/src/vm/vm_init.rs:743
cratonvm/internal/UnmodifiableSortedSet     <- vm/src/vm/vm_init.rs:743
java/util/Comparator$Native                 <- vm/src/vm/vm_init.rs:743
java/util/Enumeration$Impl                  <- vm/src/vm/vm_init.rs:743
cratonvm/stream/LazyOp                      <- native-collections/src/lib.rs:18223
java/util/ArrayDeque$Itr                    <- native-collections/src/lib.rs:35742
java/util/HashMap$KeyItr                    <- native-collections/src/lib.rs:13623
java/util/LinkedList$Itr                    <- native-collections/src/lib.rs:33133
java/util/TreeSet$Itr                       <- native-collections/src/lib.rs:43546
java/util/function/Consumer$AndThen         <- native-builtins/src/phases_late/streams.rs:3390
```

## The distinction that makes the census usable: **a request is not a failure**

The reachability screen (`FabReach.java`, 33 probes, HotSpot 33/33) shows
`HashMap` iteration, `Comparator` combinators and `Collections.enumeration` all
**passing** under `--jdk-only` — while the census above shows
`HashMap$KeyItr`, `Comparator$Native` and `Enumeration$Impl` being requested and
refused in the same run. The native asks, is correctly refused, and **the caller
recovers onto real JDK bytecode.** That is the intended behaviour of strict
mode working.

So the two instruments have opposite biases and are only sound together:

* **The census over-reports.** Every refusal appears, including the harmless
  ones that recover. Reading it alone would put 19 classes on the Phase 1
  worklist when only a handful break anything.
* **The probe under-reports.** It sees only the routes it thought to take.
  Reading it alone missed nothing here, but it cannot prove absence.

The useful quantity is the **intersection**: refused *and* not recovered from.
That is the five-family blocking set in `P1-BASELINE-20260812.md`, and it is
also why "the census is clean" and "applications work" are different claims.

A corollary for Phase 2: the 226 `native-shadows-bytecode` rows are not 226
defects. They are 226 places to *ask the question*, and the corpus is what
answers it.

## Reproduce

```
cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp . FabReach
grep -o '"kind":"[^"]*"' r.json | sort | uniq -c
grep -o '"kind":"compatibility-class-requested"[^}]*' r.json \
  | sed 's/.*"class":"\([^"]*\)".*"requester":"\([^"]*\)".*/\1 <- \2/' | sort -u
```

**Flag order matters and is silent when wrong:** placed *after* the main class,
`--jdk-only-report` is ignored — no file, no warning, exit 0.
