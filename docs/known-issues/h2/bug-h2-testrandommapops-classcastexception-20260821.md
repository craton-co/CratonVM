# `TestRandomMapOps` — `ClassCastException: String cannot be cast to Map$Entry`, not yet root-caused

## Status
**OPEN 2026-08-21.** Found while triaging
`hangs-true-vs-perfcliff-RESOLVED-20260821.md`. Not previously flagged: the
2026-08-18 census listed this class as CratonVM-side but wall-clock (`cap` at
300s, needing only >2.2x — the smallest multiplier of the 22 CratonVM-side
rows, "close to clearing"). It now clears in comparable time to HotSpot's own
~137s — and gets a wrong answer instead.

## Symptom
`org.h2.test.store.TestRandomMapOps`, default config (ZGC + JIT),
`--Xmx 1g`, real JDK 25, `--Xmx 1g`, rc=1 at 108s (93% CPU, not I/O-bound):

```
15:41:30 01:47.511 org.h2.test.store.TestRandomMapOps seed:-418228611310259706 op:1213
  java.lang.ClassCastException: class java.lang.String cannot be cast to
  class java.util.Map$Entry (java.lang.String and java.util.Map$Entry are
  in module java.base of loader 'bootstrap')
	at org/h2/test/store/TestRandomMapOps.main(TestRandomMapOps.java:46)
	at org/h2/test/TestBase.testFromMain(TestBase.java:479)
	at org/h2/test/store/TestRandomMapOps.test(TestRandomMapOps.java:57)
	at org/h2/test/store/TestRandomMapOps.testMap(TestRandomMapOps.java:68)
	at org/h2/test/store/TestRandomMapOps.testOps(TestRandomMapOps.java:231)
	at org/h2/test/store/TestRandomMapOps.assertEquals(TestRandomMapOps.java:246)
```

`TestBase.testFromMain` reruns the failing seed once it has one — this is
seed `-418228611310259706`, operation `1213` of whatever this run's op count
was, so it should be directly reproducible by fixing the seed rather than
running the fuzzer cold.

## Circumstantial evidence, not yet a root cause

Two collector-guard log lines fired in the same run, close to the crash but
not proven connected:

```
[WARN cratonvm::gc::guard] a descriptor-aware field access DESTROYED the
  value it was handed (G30-1-the-silent-reference-slot-coercion-20260817.md).
  ... species=primitive-into-reference access=read descriptor=L value=Int(0)
  class_id=664 index=2 occurrence=8192

[ERROR cratonvm::gc::guard] ...in_published_snapshot=true published_roots=425
  last_publish_at_collection=3 collections_now=4
  top_frame=org/h2/test/store/TestRandomMapOps.assertEquals pc=31
```

**Why this is not enough to call it root-caused**: the census
(`nonpassed-40-census-20260818.md` §1) explicitly warns that the
"descriptor-aware field access DESTROYED the value" WARN "appears in all 40
[failing] logs — and in 20 of 20 passing logs. It is uniform background
noise here, not a discriminator... this is the second time this shape has
nearly produced a false finding." Presence alone proves nothing.

What's different here is timing, not presence: the ERROR-level line names
`TestRandomMapOps.assertEquals` as the top frame at the moment of a root-set
publication gap, and the actual `ClassCastException` follows within the same
run. That is worth investigating, not worth concluding from — resolve
`class_id=664` first (`CRATONVM_DBG_LAYOUT=1`) and get the full coercion
backtrace (`CRATONVM_DBG_COERCION=1`) on the fixed seed before deciding
whether this is G30-1's family or an unrelated `Map.Entry` handling bug in
H2's own `MVMap` iteration.

**2026-08-22 update — a second, independent occurrence, and it argues for
caution rather than confirmation.** Rerunning the 48-class union's still-open
36-class residual on Azure (fresh `dev` tip) turned up the same ERROR-level
`in_published_snapshot=...` root-collection-gap line in
`org.h2.test.unit.TestClassLoaderLeak`'s log, immediately before ITS crash:

```
ERROR cratonvm::gc::guard: ...in_published_snapshot=false ... published_roots=0
  last_publish_at_collection=18446744073709551615 collections_now=0
  top_frame=org/h2/test/unit/TestClassLoaderLeak$TestClassLoader.<init> pc=9
```

But `TestClassLoaderLeak`'s actual failure is unrelated to this class's
`Map.Entry` cast:
`ClassCastException: class jdk.internal.loader.ClassLoaders$AppClassLoader
cannot be cast to class java.net.URLClassLoader` — a plain JDK9+ fact (the
system class loader has not been a `URLClassLoader` since JDK 9), true on
stock HotSpot 25 too, and already covered by this class's "not a CratonVM
bug" classification in the census and
`hangs-true-vs-perfcliff-RESOLVED-20260821.md`.

That is a second class where this ERROR line fires **and the crash it
precedes has nothing to do with GC roots or reference coercion** — the same
shape the census's §1 already warned about for the WARN-level sibling line
("appears in all 40 failing logs and in 20 of 20 passing logs... the second
time this shape has nearly produced a false finding"). One clean corroborating
occurrence would have strengthened the root-cause hypothesis; a second
occurrence next to a confirmed-unrelated, confirmed-not-a-CratonVM-bug crash
weakens it instead. Treat the ERROR line as circumstantial in BOTH directions
until `CRATONVM_DBG_LAYOUT=1`/`CRATONVM_DBG_COERCION=1` actually resolves what
it is naming, not as evidence either the `TestRandomMapOps` bug or this line
share a mechanism.

## Reproducing
```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home <jdk25> --Xmx 1g -c "$CP" org.h2.test.store.TestRandomMapOps
# on the failing seed specifically, once TestBase exposes a seed-pinning knob
# (check TestRandomMapOps.java / TestBase.java for one — not yet confirmed)
```

## Related
- `docs/known-issues/gc/G30-1-the-silent-reference-slot-coercion-20260817.md`
  — the WARN family this may or may not belong to.
- `docs/known-issues/h2/nonpassed-40-census-20260818.md` §1 — the
  methodology warning about over-reading this WARN shape.
- `docs/known-issues/h2/correctness-issues-consolidated.md` — indexes this
  finding alongside the rest of the 48-class union's correctness results.
