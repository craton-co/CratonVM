# In-VM `javac` intermittently dies in `Modules.setupAllModules` — CLOSED 2026-08-06

**Status:** CLOSED, **not reproduced**, and both hypotheses the report named are
now refuted by direct measurement rather than by more clean runs.

This is closed on evidence, not by a fix: nothing was found to fix. What
changed since the previous pass is that the observation now has an affordable
reproduction budget and its two named suspects have been tested directly.

## What was seen (2026-08-05, unchanged)

```
cratonvm --real-jdk --java-home /data/data/jdk25-real \
  -cp /data/data/jdk25-real/lib/jrt-fs.jar com.sun.tools.javac.Main \
  -nowarn -d <outdir> @<file-list>
```

exits **4** (javac's `EXIT_ABNORMAL` — an unexpected `Throwable` escaped),
writes zero class files, and takes ~6 s against the ~60 s a successful run of
the same corpus takes. Seen 3 times in ~10 runs, then 0 times in 79 deliberate
attempts the same day. The head of the exception was never captured; the tail:

```
com.sun.tools.javac.code.Symbol$ClassSymbol.complete(Symbol.java:1472)
com.sun.tools.javac.comp.Modules$1.complete(Modules.java:652)
com.sun.tools.javac.code.Symtab.lambda$enterModule$0(Symtab.java:865)
com.sun.tools.javac.code.Symbol.complete(Symbol.java:703)
com.sun.tools.javac.comp.Modules.lambda$setupAllModules$2(Modules.java:1285)
com.sun.tools.javac.comp.Modules.setupAllModules(Modules.java:1309)
com.sun.tools.javac.comp.Modules.initModules(Modules.java:239)
com.sun.tools.javac.main.JavaCompiler.initModules(JavaCompiler.java:1047)
```

## The measurement that made volume affordable

Zero class files and ~6 s against ~60 s means the failure is entirely inside
module bootstrap, *before any source file is read*. So a **one-file** corpus
exercises the identical path: measured here at 2.4–11 s per run (median 4.3 s),
i.e. the same order as the failures themselves, and **~1.3 s amortised at width
4**. That is why the previous pass could afford 79 attempts and this one
afforded thousands — the expensive 60-file corpus was never load-bearing for
this failure window.

`/data/tmp/jm/jmquick.sh` is the one-file loop, `/data/tmp/jm/jmloop.sh` the
original 60-file one. Both keep each run's full stderr and delete it only on
success, so the next failure captures its own head-of-exception. The corpus is
regenerable (`gen_jm.py`: 60 classes × 40 trivial methods, ASCII only).

## Suspect 1 — the jimage reader: NOT ON THIS PATH

`--dump-native-registry` over a complete javac invocation reports
`jdk.internal.jimage.NativeImageBuffer.getNativeMap` with **zero invocations**.
With `-cp lib/jrt-fs.jar` the module image is read by real JDK bytecode; the
whole compile fires only ~10 natives, almost all of them `Unsafe` field
offsets plus a single `ByteBuffer.wrap`/`array`/`hasArray`/`remaining` group and
one `CharBuffer.allocate`.

This also disposes of a hypothesis this pass raised and did not spend a build
on: that native reads the ~150 MiB image into a Java `byte[]` behind a
process-global cache, which would have been a large allocation exactly under
the memory pressure the original failures had. It is not reached.

## Suspect 2 — identity-hash walk order: REFUTED

The report's own reasoning: javac's `Symbol`s do not override `hashCode()`, so
`setupAllModules`' walk is ordered by identity hash, which "moves with
allocation addresses and therefore with GC timing".

Order varying *between* runs is normal — HotSpot does it too and javac
tolerates it. The failure mode that would actually corrupt a walk is an object
whose identity hash changes *within* a run, when the collector relocates it.
`probes/IdentityHashStabilityProbe` (4,000 objects, four churn-and-collect
phases, checked against both an `IdentityHashMap` and a `HashMap`) reports
`changedHashes=0 idmapLost=0 hashmapLost=0` on **both** the default
Generational collector and G1, byte-identical to HotSpot. CratonVM mints the
hash from a counter into the object header and every mover copies the header
verbatim (`gc/src/gen_heap.rs::identity_hash_code`), so it is stable by
construction.

## Suspect 3 — host load and memory: exercised, clean

## The runs

All on the Azure Linux host, on `fix/cbwrap-javac-known-issues-20260806`
(itself a superset of `dev`), on a box whose load average sat between 20 and 70
throughout — the original failures were at load ~30.

| arm | shape | runs | failures |
|---|---|---:|---:|
| A | one-file, width 4 | 100 | 0 |
| B | one-file, width 12 | 240 | 0 |
| C | one-file, width 8, `--Xmx 256m` | 160 | 0 |
| D | one-file, width 8, `--nojit` | 120 | 0 |
| E | one-file, width 8 | 2000 | 0 |
| F | **the original 60-file corpus**, width 4 | 100 | 0 |
| G | one-file, width 8, `-XX:+UseG1GC` | 800 | 0 |
| H | one-file, width 8, `--Xmx 96m` | 400 | 0 |
| I | one-file, width 8, `--Xmx 96m -XX:+UseG1GC` | 400 | 0 |

**4,320 runs, 0 failures**, plus the previous pass's 79. Arm F is the exact
original shape, so the cheap arms are not the only evidence. At the observed
rate of 3-in-10, a single clean run of 4,320 has probability ~10^-670; even at
1-in-1000 it would be ~0.013. Whatever produced those three failures is not a
property of this binary running this compile.

## What is still uncontrolled

The report recorded one variable it could not dismiss: all three failures used
output directory `/data/tmp/jout2`, and every attempt after switching
directories succeeded. Every run here used a fresh per-run output directory
under a different root, so that variable is still untested. It is recorded
rather than dismissed — but note the directory was `rm -rf`'d and recreated
before each of the three failures.

Also worth keeping: all three failures were on a binary with `String.hashCode`
re-registered as `Intrinsic`, a configuration that does not exist on `dev`. The
previous pass rebuilt that exact configuration and got 35 clean runs, so the
correlation did not survive — but it means the original three samples were not
taken on a shipped binary.

## Reopening it

Keep the harness. If `EXIT_ABNORMAL` recurs, the run's full stderr survives in
`/data/tmp/jm/log-<tag>/run<id>.err` — including the head of the exception,
which is the one thing this bug has never had.
