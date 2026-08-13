# quarkus — FAIL/HANG classes needing investigation (index)

**Batch 01 is CLOSED (2026-08-13); no page in this index is open.**

**7 unique classes** (union of FAIL/HANG across all 3 GC-variant runs, 2026-08-12), split into 1 pages of up to 15 classes each so multiple people can pick up a page without duplicating work. This started as a raw class list plus a repro command template; batch 01 has since been investigated and closed (below).

Found during a **partial**, STOPPED-early 3-GC-variant (default/G1/ZGC) full-scope run on Azure host `azureuser@20.80.105.49` (~6,377-class harness, 2 shards/variant, stopped deliberately after ~12hrs / ~14% coverage to free host resources -- see `docs/known-issues/quarkus/run-checkpoint-20260812.md` for exact per-shard stop points and how to resume the remaining ~5,549 classes). PASS rate on the ~2,645 classes actually attempted was 99.5%+ -- this is a SMALL, not-yet-representative sample: only 7 unique classes hit HANG across all 3 variants combined, and zero hit FAIL. Do not read "only 7 classes" as "quarkus is nearly clean on CratonVM" -- ~5,549 of 6,377 classes were never reached.

## Pages

- **batch 01 — CLOSED 2026-08-13.** All 7 classes PASS on default / G1 / ZGC.
  None of them hung: run standalone each finished at ~110s against HotSpot's
  ~17s, and that fixed cost was classpath resource enumeration. Three defects
  came out of it — `getResources` re-deriving the whole classpath per probe
  (a canonicalize cache whose 1024-entry FIFO could not hold a 4230-entry
  classpath, plus an eager enumeration where the JDK's is lazy), a hard-coded `LoggingSetupRecorder.initializeLogging` descriptor the
  library had outgrown (which had been silently skipping whole test classes),
  and a native xerces scanner reading `XMLChar.CHARS` unpinned and
  uninitialized. Write-up: fixed-suite-bugs/quarkus/investigate-batch-01-FIXED.md
  (internal).

## Repro

```bash
# on azureuser@20.80.105.49, /data/cratonvm/apps/quarkus-suite-runner
echo <ClassName> > /tmp/one.txt
./run-quarkus-suite.sh --list /tmp/one.txt --shards 1 --timeout 180 --out /tmp/repro   --bin /data/cratonvm/target-zgc/release/cratonvm-quarkus-default-wrapper.sh
# swap the -default- wrapper for -g1- / -zgc- to pick the GC variant: the
# wrapper IS the selector. There is no --gc flag; passing one prints the usage
# text and runs nothing, which is what the retired batch-01 page told people to
# do.
# HotSpot cross-check: run the same class/classpath under stock HotSpot (JDK 25)
```

## Reading a quarkus PASS

The harness scores a class PASS when `found>0 && failed==0`, so a class whose
JUnit extension threw in `beforeAll` — discovered, never started — is recorded
as a pass. Batch 01 found two classes in exactly that state. **A result with
`started=0` is "unknown", not "green"**; check it against HotSpot, which
reports `started=0` for the `@QuarkusTest`-style classes this flat-classpath
harness structurally cannot bootstrap and `started=1` for everything else.
