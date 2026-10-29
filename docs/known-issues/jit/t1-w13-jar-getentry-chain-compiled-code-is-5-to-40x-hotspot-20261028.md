# t1 W13: the compiled `JarFile.getEntry` chain is 5-10x HotSpot (optimizing) and ~40x (single-pass)

Filed 2026-10-28 by t1 wave 13, lane L71, while retiring
`docs/internal/fixed-bugs/interpreter/t1-L71-jar-get-entry-bimodal-and-define-native-cost-FIXED-20261028.md`,
which root-caused the "bimodal" `get-entry` row to the tier transition. Measured on the Linux host 64.177
(dev 214b45112c + the wave-13 branch, `--jdk-only`); not fixed.

## Symptom

`JarFile.getEntry(name)` over a 400-entry jar, ns a call:

| tier the getEntry chain runs in | CratonVM | HotSpot 25.0.4.1 |
|---|---|---|
| interpreter (first 400 calls) | 44 000-53 000 | 3 900-4 900 |
| single-pass bodies (`L5W117JarClassPhaseSplitBench` rounds 1-3) | 13 000-15 500 | 270-430 (C1) |
| optimizing bodies, compiled caller (`T1W8I71GetEntryRounds` `method-loop`, rounds 7-19) | 1 000-1 100 | 180-420 |
| optimizing bodies, main's OSR loop (`main-loop`) | 2 000-2 100 | 270-470 |

In `L5W117` the optimizing compiles of `ZipFile$Source.getEntryPos`, `ZipFile.getEntry`,
`JarFile.getJarEntry` and `JarFile.getEntry` land in round 4 (the second `put` under
`CRATONVM_DBG=jit-compiled`). So the row's median of 7 rounds is a single-pass round (~14 us, 38x HotSpot)
on this host every run, and on the Windows box on either side of that line: the "bimodal" run.

## Open questions for the JIT owner

1. Why is the single-pass chain ~40x C1? The chain is a hash, a bucket walk and a byte compare
   (`ZipCoder$UTF8ZipCoder.checkedHash`, `Source.getEntryPos`, `ZipCoder.compare`) plus a
   `JarFileEntry.<init>`. Profile a run held in the single-pass tier (frame-pointer build,
   `CRATONVM_JIT_PERF_MAP=1`) and bucket the samples by the runtime entry below each `[c1]` frame
   (`docs/known-issues/` perf-gap recipe). Calls that leave the compiled code (MIC misses, a native door per
   compare) are the likely suspects.
2. The optimizing floor (5-10x): the same profile with the chain in the optimizing tier.
3. Whether the optimizing compiles can land sooner for a method this hot (2 800 calls before round 4).

## Verify

`L5W117JarClassPhaseSplitBench` with the per-round array printed after the loop (`ROUNDS get-entry
[...]`, as in the retired page). Rounds 1-3 should move toward the optimizing floor, and the floor in
`T1W8I71GetEntryRounds` toward HotSpot.

## 2026-10-29 exact-tree follow-up (OPEN)

`T1W8I71GetEntryRounds` completed all 20 rounds and found all 400 entries in HotSpot and both CratonVM
modes. Timings were highly variable on the shared host: HotSpot's `method-loop` ranged from 287 ns/op to
17.1 us/op; JDK-only ranged from 1.5 us/op to 55.8 us/op; compatible ranged from 1.3 us/op to 91.9 us/op.
`L5W117JarClassPhaseSplitBench` preserved every deterministic row (`classes`, `read-bytes-ok`, both checksums,
delegate misses, and resource hits all matched 400/80200 as expected), but e2e measured 167 us/class on
HotSpot, 3.24 ms in JDK-only, and 2.62 ms in compatible. These loaded-host timings cannot settle the requested
single-pass and optimizing-floor profile gates; this issue remains OPEN.

## 2026-10-29 final-dev-tree follow-up (OPEN)

The rebased tree `71c6acdd97bcefce681440e3ff7d1a620699b783` built successfully. `T1W8I71GetEntryRounds`
again found all 400 entries in all three runtimes, but `method-loop` varied from 153 ns to 1.25 us on
HotSpot, 925 ns to 20.3 us in JDK-only, and 841 ns to 16.9 us in compatible mode. The class-phase probe
matched deterministic rows (`classes=400`, byte-read and resource counts 400, checksums 80200); e2e was
19.9 us/class on HotSpot, 207 us in JDK-only, and 254 us in compatible. Shared-host variability and the
remaining gap leave the requested compiled-floor gate open.

## 2026-10-29 exact-source recheck (OPEN)

On the current task source, `T1W8I71GetEntryRounds` completed all 20 rounds and found all 400 entries
in HotSpot, JDK-only, and compatible modes. After warm-up, HotSpot main-loop samples were 0.15–0.32
us/op and method-loop samples 0.15–0.24 us/op. JDK-only method-loop rounds 15–19 were 0.84–0.88
us/op; compatible rounds 15–19 were 0.84–0.94 us/op. The main-loop samples and early rounds were
noisier, so these are shared-host observations rather than a pricing baseline. The class-phase probe
preserved all deterministic facts and checksums; e2e was 20.97 / 157.87 / 246.76 us/class for
HotSpot / JDK-only / compatible. The compiled chain still misses the acceptance target; this page
stays OPEN.

## 2026-10-29 warm C2 profile (OPEN)

With the current release binary, the probe ran 200 rounds pinned to CPU 3; the JDK-only
`method-loop` rounds 192–199 were 0.80–0.96 us/op and all 400 entries were found. A delayed,
user-instruction `perf record` captured 4,052 samples after startup. The perf map attributed 586
sample stacks to compiled Java bodies; `ZipFile.getEntry` C2 appeared in 219 stacks,
`ZipFile$Source.checkAndAddEntry` C2 in 226, and `ZipCoder$UTF8ZipCoder.checkedHash` C2 in 48.
The hot methods are admitted and run at C2; the remaining measured floor is inside the compiled
lookup chain, rather than a failure to reach the optimizing tier. The profile does not yet isolate
one code-generation or helper change that is safe and measurably improves this chain. The acceptance
gap therefore remains OPEN. Capture: `/tmp/t1-getentry-cvm-inst-20261029.data`, its `.script`, and
`/tmp/t1-getentry-cvm-inst-20261029.out` on host 64.177.

## 2026-10-29 final exact-tree recheck (OPEN)

On the exact rebased task tree, `T1W8I71GetEntryRounds` again found all 400 entries. With 200 rounds
pinned to CPU 3, JDK-only method-loop rounds 192–199 measured 1.14–1.50 us/op; HotSpot's matching
rounds were 0.085–0.144 us/op. These shared-host values show the remaining gap but are not quiet-host
pricing. The delayed user-instruction profile recorded 10,044 samples and 1,372 stacks containing
Java JIT frames. C1 `ZipFile$Source.checkAndAddEntry` contributed 633 samples, C1
`ZipFile$Source.initCEN` 482, C2 `ZipFile.getEntry` 272, C2 `ZipFile.getZipEntry` 150, C2
`JarFile.getEntry` 117, and C2 `ZipCoder$UTF8ZipCoder.checkedHash` 86.

`CRATONVM_DBG_JITC=1` confirms `checkAndAddEntry` has a published C2 body, but the stale-call site
from `initCEN` is not rebound: the current body is outside rel32 reach of that site. The site has
no safe existing rel32 target for the replacement; the current implementation only handles rel32
and the recognized `MOV RAX, imm64 ; CALL RAX` form. This identifies a rebind limitation, not proof
that it alone explains the throughput floor. No out-of-range patch or trampoline was introduced;
the optimization and quiet-host acceptance gates remain open. Logs: `/tmp/t1-final-getentry.data`,
`/tmp/t1-final-getentry.out`, and `/tmp/t1-final-getentry-jitc.out` on host 64.177.
