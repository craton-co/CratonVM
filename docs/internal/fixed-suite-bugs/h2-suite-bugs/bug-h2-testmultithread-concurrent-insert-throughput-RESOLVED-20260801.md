# `TestMultiThread.testConcurrentInsert` — the insert claims were a debug build; the class still fails, in a DIFFERENT method

## Status

**THIS PAGE'S CLAIMS ARE RESOLVED; THE CLASS IS NOT.** Re-measured against a
**release** binary of `dev` @ `c8a3ba181d`. Every quantitative claim on this page
came from a debug build and does not survive re-measurement — but
`org.h2.test.db.TestMultiThread` still fails, and the failure has **moved to a
different method**, so do not read this page as "the class passes".

| original claim | verdict |
| --- | --- |
| single-threaded `INSERT`+`commit` is **258x** HotSpot | **wrong** — ~25-30x. The measurement used a debug build. |
| `testConcurrentInsert` fails on `job.get(5, TimeUnit.MINUTES)` | **does not reproduce** — the insert shape (25x1000) finishes in 79-155 s |
| **worse-than-HotSpot scaling** (591x at 4 threads, 348x at 25) | **refuted** — CPU cost per row is FLAT across 1-8 threads |
| intermittent `CloneNotSupportedException` at 25 threads | **not reproduced**, 0 of 6 runs |

## The class still fails — in `testConcurrentUpdate`, not `testConcurrentInsert`

Running the real class on the release binary (`--Xmx 1g`, HotSpot control on the
same host in the same minute):

```
HotSpot    real 0m13.1s   user 0m18.6s   rc=0
cratonvm   real 13m3.9s   user 6m32.1s

Exception in thread "main" java/util/concurrent/TimeoutException
    at org/h2/test/db/TestMultiThread.testConcurrentUpdate(TestMultiThread.java:382)
```

The `TimeoutException` this page documents was at
`testConcurrentInsert(TestMultiThread.java:327)`. Execution now gets **past**
that method and dies in `testConcurrentUpdate` (25 threads x 10 000 objects,
UPDATE rather than INSERT) instead. That is a real change of failure site, and
`testConcurrentUpdate` is a different workload that this page never measured —
it needs its own investigation, not an inherited conclusion.

**Also surfaced in the insert method, and NOT present on HotSpot:**

```
WARN NoSuchMethodError method="java/lang/Object.next()Ljava/lang/Object;"
     caller="org/h2/test/db/TestMultiThread.testConcurrentInsert()V @pc=197"
```

An `Iterator.next()` dispatched against `java/lang/Object` — the
interface/array-receiver dispatch family (compare
`bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`).
It is logged at WARN and the run continues, so something recovers, but it is a
genuine dispatch defect and worth its own bisection.

## Why the original numbers were wrong

Two independent measurement faults, each worth roughly an order of magnitude.

**1. The binary was a debug build.** The Measurement section said so in passing
(`dev @ 4a48f12cb6 (debug build)`) and then quoted its ratios as VM defects. A
debug cratonvm is ~5-10x slower than release. Re-run on release, same host,
same JDK 25, same `--Xmx 1g`, 1 thread x 1000 INSERT+commit:

| | wall_ms |
| --- | --- |
| HotSpot | 207 |
| cratonvm **release** | 6 634 |

**2. Multi-threaded wall-clock on this host is noise.** The box is 16 cores
shared with 15-40 other cratonvm sessions. The *identical* shape
(`4 1000`, `--Xmx 1g`, one binary) measured **152 776 ms** in one pass and
**22 812 ms** twenty minutes later — 6.7x apart. HotSpot finishes the same shape
in under a second so it barely samples the contention; cratonvm runs for tens of
seconds and absorbs all of it, which manufactures a "cratonvm scales worse"
result out of scheduler pressure. This page's 591x/348x rows are that artifact.

> Do not quote a multi-threaded wall-clock ratio from this host. Measure CPU
> time (`/usr/bin/time -f '%U user %S sys'`), round-robin the configurations,
> take the min of N, and record `uptime` next to every number.

## What is actually true: a flat ~25-30x constant factor

Load-robust measurement — host at load 29-32, configurations round-robined,
min of 3 reps, CPU (user) time with process startup subtracted:

| threads | cratonvm ms CPU/row | HotSpot ms CPU/row | ratio |
| --- | --- | --- | --- |
| 1 | 4.69 | 0.19 | ~25x |
| 2 | 4.78 | 0.23 | ~21x |
| 4 | 5.32 | 0.20 | ~27x |
| 8 | 5.56 | 0.18 | ~31x |

**cratonvm's CPU cost per row is flat across 1-8 threads (+18% end to end).**
The VM does not burn extra work as concurrency rises — no lock-contention
blowup, no GC blowup. Total CPU for 8 000 rows (45.2 s) is what 8x the
single-thread cost predicts (44.1 s).

So there is no scaling wall. There is one roughly constant ~25-30x per-row cost,
which is the general interpreter/dispatch gap and is not specific to concurrent
inserts.

**One real residual, small:** parallel *efficiency* is worse than HotSpot's. At
8 threads cratonvm used 1.50 CPUs (45.2 s CPU / 30.1 s wall) against HotSpot's
2.7, measured on the same box in the same minute. That is ~1.8x, and it is
dwarfed by the constant factor. Worth knowing; not worth a page of its own.

## The profile is flat — there is no single hotspot to fix

`perf record -F 199 -g --call-graph=fp` over a warmed single-threaded run
(9 006 samples):

```
 4.84%  __memcmp_evex_movbe            (libc)
 3.78%  _mi_page_malloc_zero
 2.55%  interpreter::execute_frame_from_index
 2.19%  vm_exec::invoke_on_class_shared_inner
 2.17%  NativeMethodRegistry::slot_for_exact
 1.64%  invoke::execute_invokevirtual_cached
 1.60%  jit::helpers::jit_invoke_virtual_mic
 1.47%  invoke::jit_method_calls_native_shadowed
 1.40%  invoke::force_native_over_real_jdk_bytecode
 ... long tail ...
```

Two clusters are worth naming for whoever attacks the constant factor:

* **~5.6% native-registry lookup** — `slot_for_exact` + `slot_index_for_key` +
  `compute_jit_key_hash` + hashbrown search.
* **~3.8% per-call "should a native beat bytecode?" decisions** —
  `jit_method_calls_native_shadowed` + `force_native_over_real_jdk_bytecode` +
  `should_force_registered_native_over_bytecode`. These are `matches!` chains
  over class/method name *strings*, evaluated per dispatch, which is the most
  likely source of the 4.84% `memcmp` at the top.

## Suspects this page named, both dead

* **`jit_activation`'s global `Mutex`** — already gone. Replaced by per-thread
  activation tables on 2026-07-31 (`types/src/jit_activation.rs`); the global
  lock survives only behind `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX=1` for A/B.
* **The H2 MVStore write path's `synchronized` regions** — not implicated. The
  CPU-per-row flatness rules out contention, and `--nojit` (which removes JIT
  frames, conservative roots and the STW takeover scan entirely) scales
  identically to JIT-on.

Also ruled out, each with a one-variable experiment:

| hypothesis | lever | result |
| --- | --- | --- |
| ~~`org/h2/` JIT ban keeps H2 interpreted~~ | ~~`CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`~~ | **WITHDRAWN 2026-08-02 — a null A/B.** There is no `org/h2/` package ban: `CRATONVM_DBG_JIT_COMPILED=1` counts **27** `org/h2/…` methods compiled on the default build and **26** with the flag set. Both arms of "7 210 vs 6 634 ms" were the same configuration. |
| young-gen heap pressure | `--Xmx` 1g/2g/4g/8g | no trend (22.8/16.8/21.6/14.6 s) |
| young-GC livelock | `CRATONVM_DBG_YOUNG_TRIGGER=1` | `live` 63→172 MB vs `threshold=230MB`, never reached |
| STW cross-thread takeover cost | `CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200 | no effect (12.7/13.9/12.3 s) |
| JIT-root / conservative-scan path | `--nojit` | identical curve |

## The `CloneNotSupportedException` residual — not reproduced

The note appended to
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`
recorded one 25x1000 run in which all 25 threads failed on `COMMIT`.

**Round 1 of this re-run is VOID, and the reason is worth reading.** Six clean
runs (`failed=0`) were obtained with `CRATONVM_DBG_SWEEP_ZERO=1` armed —
following the sibling doc's own recommended recipe, so "the next occurrence
self-diagnoses". But that flag sets `retain_dead_objects` in
`gen_heap.rs::sweep_young_non_moving`, which **switches the young sweep from the
8-worker anchored parallel walk to the sequential walk and stops dead spans
being coalesced**. That is a semantics-level change to the exact code under
suspicion, so those runs never exercised the sweep the failing run used. A
"clean" result there means nothing.

> Any `CRATONVM_DBG_SWEEP_ZERO` / `DBG_A2` / `DBG_SWEEP_CENSUS` /
> `DBG_WATCHREF` run is testing a DIFFERENT young sweep. Reproduce first with no
> flags; only then instrument. This is also a live candidate for why
> `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md` went from ~40%
> reproduction to 0/18 "the next day on current dev + instrumentation" and
> concluded the host had changed.

**Round 2, uninstrumented — `failed=0` in 8 of 8** runs of the exact shape that
produced it (25 threads x 1000 rows, release binary, `--Xmx 1g`, **no debug
flags at all**), wall 43.7-130.2 s, host load falling 63 -> 16 across the set.
This one does exercise the real 8-worker parallel sweep, so unlike round 1 it is
a valid negative.

It is still **not** a claim that the bug is fixed. The original was ONE
occurrence in one run; eight clean runs of a rare event is weak evidence of
absence, and no positive control was run to prove the probe's error path still
fires on this binary — a silent canary and a clean run look identical
(the probe does print `first error:` plus a stack, and did so on the original
occurrence, but that is not the same as demonstrating it on THIS build).

The residual belongs to the array-receiver dispatch family tracked on
`bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`,
not to this page's throughput subject — it was only ever noted here in passing.
If it recurs, the most likely home is the open premature-reclamation bug in the
non-moving young sweep
(`docs/known-issues/h2/bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`),
whose signature — a live object's header reading back as something else — is the
same family. **Reproduce it with no flags first**; the diagnostic that page
recommends is the one that voided round 1 here.

## Bugs found while investigating

**1. `java.util.Random.nextGaussian()` violated its spec — FIXED here.** It
computed the polar method's pair and discarded the second variate instead of
caching it, in **both** live registrations
(`native-collections/src/lib.rs` and `native-builtins/src/securerandom.rs`).
That advanced the LCG at twice the specified rate, so every *seeded* sequence
diverged from a conforming VM. Differential proof — first six `nextGaussian()`
values of `new Random(42)`, as raw bits:

```
HotSpot   ...750  ...127  -4616641179245592382  -4615707776640798080  4598733263062401967  4604341753479877564
PRE-FIX   ...751  -4616641179245592382  4598733263062401967  (diverges)
```

the pre-fix VM emits HotSpot's values **1, 3, 5** — each pair's first variate,
with the partner thrown away. Fixed, with a regression test pinning the exact
stream (`next_gaussian_matches_jdk_seeded_sequence`). `PageStorageProbe`'s
seeded checksum now matches HotSpot exactly (`-996036681280942953`, was
`1038429237398187486`).

**2. ~~A deterministic JIT-only zero-length-array defect — filed separately.~~**
**WITHDRAWN 2026-08-02.** It does not reproduce — ~250 probe rounds and 25 runs
of the real class, on current `dev` *and* on `8d837f1244`, the exact commit this
page was written from. `TestMemoryEstimator` fails at HotSpot's own rate on an
unseeded `Random` (cratonvm 2/25, HotSpot 3/25) and there is no `org/h2/` ban
for it to block. See
`jit-zero-length-array-20260801-WITHDRAWN.md` in this directory.

## What is left open

*(Resolved 2026-08-02 — see `docs/known-issues/h2/` for where each went.)*

1. ~~**`testConcurrentUpdate` times out.**~~ Measured. The class is not
   deterministically broken: on a quiet host it **passes** in 727 s against
   HotSpot's 7.3 s, and H2's own timeouts (a 10 s `LOCK_TIMEOUT`, a 5 min
   `job.get`) trip in a different method on each run. The UPDATE path — unlike
   the INSERT path this page measured — does have a real contention component:
   CPU per update **doubles** from 4 to 25 threads (3.6 → 7.8 ms) while
   HotSpot's falls (0.41 → 0.15). Now
   `bug-h2-testmultithread-concurrent-update-timeout.md`.
2. ~~**`NoSuchMethodError: java/lang/Object.next()`**~~ — reproduced with a
   receiver dump, promoted to its own page:
   `bug-h2-blocked-frame-classid0-dispatch-miss.md`. It is the ambiguous
   `ClassId(0)` face, same family as
   `bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`.
3. **The flat ~25-30x constant factor** — still open, and still the biggest
   number here. See the profile section for the clusters worth attacking first;
   the 25-thread UPDATE profile on the successor page adds a contended
   `ClassManager` rwlock (`lock_shared_slow` 1.47%) that the 1-thread profile
   could not show.
4. ~~**the JIT zero-length-array defect**~~ — withdrawn, see above.

## Reproducing

```bash
# the probe (release binary, not debug)
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<probe-dir>" H2InsertScaleProbe /abs/dir 1 1000

# the class this page is named for
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Probe source: `docs/internal/repros/h2-insert-scale-20260731/H2InsertScaleProbe.java`
(H2 2.x rejects a relative database path — pass an absolute one).
