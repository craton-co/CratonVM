# The H2 UPDATE path costs ~30-60x HotSpot's CPU per update, and ~87x over the whole class

## Status
**OPEN (2026-08-02).** Successor to the retired
`bug-h2-testmultithread-concurrent-update-timeout` write-up, which is retired
because everything on it that was a *defect* is closed: the class died in three
seconds on dev tip (two IR-tier JIT bugs, fixed), H2's own `LOCK_TIMEOUT` /
`job.get` timeouts were the slowness surfacing rather than a bug, and the
`CloneNotSupportedException` residual moved to
`bug-h2-classid0-stale-address-family.md`.

What is left is this page: a constant factor, re-measured on shapes that can
carry it, and a thread-scaling question this host cannot answer. It is
deliberately **not** filed as "a bug" — the previous framing ("≈100x. That is
the bug.") pointed three sessions at a problem no single fix could match.

## Severity
**MEDIUM.** No incorrect behaviour, but not benign either. The class takes
20-45 minutes of CPU where HotSpot takes 17 seconds, and H2's internal
`LOCK_TIMEOUT` is a **wall-clock** 10 s — so on a busy host the gap turns into
an outright test failure rather than just a slow one. One of three verification
runs came back `rc=1` with `Timeout trying to lock table "TEST"` at
`TestMultiThread.java:414`, on a host at load 57-156 whose system time exceeded
its user time. The class is neither deterministically broken nor
deterministically green, and it will stay that way until the gap closes.

## The numbers

`H2UpdateScaleProbe` models `testConcurrentUpdate` exactly: same `NUMBER(18,0)`
PK schema, same 10 000-row `MERGE` seed, same
`UPDATE account SET balance=? WHERE id=?` + `commit` inner loop, same
`LOCK_TIMEOUT=10000`. **Both shapes below do 10 000 updates**, so the work term
dominates the baseline; 0-update runs of the identical shape are interleaved as
ordinary arms and subtracted. Median of 3, load 15-35 recorded per run.

| shape (10 000 updates) | HotSpot jdk-25 | cratonvm | ratio |
| --- | --- | --- | --- |
| 4 threads × 2500 | 0.17 - 0.22, median **0.21** | 3.8 - 9.3, median **6.6** | ~**31x** |
| 25 threads × 400 | 0.15 - 0.19, median **0.17** | 7.5 - 10.6, median **9.5** | ~**58x** |

(cratonvm figures are the pre-fix arm, n=6 and n=9 runs across four campaigns at
loads 8-43; the post-fix arm is ~10 % lower, see the retired page's A/B.)

The whole-class number is the trustworthy one, because it is a single comparison
of two runs on one host in one hour rather than a difference of two large
quantities: **1303-1506 CPU-s vs 17.3, ≈75-87x** over three cratonvm runs.

### The thread-scaling slope is NOT resolved, and the old page's was not either

The retired page claimed cratonvm's CPU per update *doubles* from 4 to 25
threads (3.6 → 7.8) while HotSpot's falls (0.41 → 0.15), and named that as the
UPDATE path's distinguishing feature against the INSERT half's flat scaling.

**That does not reproduce.** Across four campaigns the per-rep 25t ÷ 4t ratio
came out 0.84, 0.98, 1.35, 1.47, 1.56, 1.67, 1.78, 2.27 — it does not even hold
its direction. The medians give 1.44x, but the two bands overlap outright
(4 threads 3.8-9.3, 25 threads 7.5-10.6), so the median is not evidence.

The reason is structural rather than bad luck, and it is worth writing down
because it applies to every cross-thread-count comparison on this host: **at
equal total work the two shapes have very different wall durations** — ~10
minutes at 4 threads against ~1 minute at 25 — so running them back-to-back
inside a rep does not make them see the same load. Pairing removes a level
shift; it cannot remove two arms sampling different load windows.

What WOULD settle it, and has not been done: a 4-thread arm doing ~100 000
updates (~11 CPU-minutes of work against a ~40 CPU-s baseline, a 16:1 ratio
instead of 1.5:1), interleaved with a 25-thread arm of the same total work, on a
host under 10 % load or on a dedicated one. Everything smaller has been tried.

Until then the honest statement is the first table: **~30x at 4 threads and
~60x at 25**, with the growth between them real-looking but unproven.

## Where the CPU goes, and why no symbol on this list is the answer

`perf record -F 199 -g --call-graph=dwarf`, 25 threads × 1000 updates, 27 K
samples, `--sort symbol`:

| cluster | share | symbols |
| --- | --- | --- |
| Rust-side allocation | 5.9% | `_mi_page_malloc_zero` |
| dispatch + JIT precedence | ~12% | `invoke_on_class_shared_inner` 2.42, `execute_invokevirtual_cached` 1.83, `InvokeCache::get` 1.45, `try_jit_compile_callee` 1.38, `jit_invoke_virtual_mic` 1.18, `force_native_over_real_jdk_bytecode` 1.06, `virtual_dispatch_target_cached` 1.02, `find_method_recursive` 1.01 |
| GC conservative root scan | 6.6% | `native_stack_has_jit_frame` 2.76, `scan_one_frame` 2.01, `is_object_address` 1.87 |
| native-method registry lookup | 6.0% | `slot_for_exact` 2.12, `__memcmp_evex_movbe` 2.79, hashbrown search 1.13 |
| ClassManager lock | 5.2% | `RawRwLock::lock_shared_slow` 1.47, `OrderedPlRwLock::read` 1.41, `load_class_concurrent` 1.40 |
| interpreter | 3.0% | `execute_frame_from_index` |

That is ~39 % of the profile. **Removing all of it is under 2x, against 30-87x.**
Treating the list as a bug list is the mistake this page exists to stop
repeating.

The named `ClassManager` target from the old page **is done**: the invoke slow
path took two `read()` guards on the same `Class` per call and now takes one,
worth a median ~10 % of CPU per update (10 paired runs; range 0.74-1.25, so a
single pair on this host proves nothing).

The two entries that are genuinely *scaling* rather than constant-factor work,
and so are the next targets:

* **`load_class_concurrent` at 1.4 % in steady state**, hundreds of seconds after
  warm-up. Nothing should be resolving classes then; find out what is.
* **the conservative root scan** is per-thread-stack work per collection, so it
  grows with (threads × collections). Every young collection in this workload
  falls back to the non-moving sweep — `reason=unregistered-jit-frame-on-stack`,
  `compiled-frame-oop-not-published`, `innermost-rbp-belongs-to-unguarded-callee`
  — which is its own question and has its own pages.

## Setup cost

10 000 `MERGE` + VM start + H2 class load, single-threaded: HotSpot **2.2-3.0
CPU-s**, cratonvm **36-54 CPU-s** on this host at load 15-30. The spread is the
host, not the VM: four runs of the identical 0-update shape, minutes apart, came
back 29 / 36 / 43 / 50 CPU-s. A flat tax on every H2 run, and the reason a
"1-second" H2 test costs half a minute here.

## What is ruled out (do not redo)

* **There is no `org/h2/` JIT package ban to lift.** Measured 2026-08-02 with
  `CRATONVM_DBG_JIT_COMPILED=1`: **27** `org/h2/…` methods JIT-compile on the
  default build, **26** with `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`. The flag is a
  no-op for this workload, so the retired insert page's "lifting the ban made it
  ~9 % worse" was a **null A/B** — two identical configurations — and is
  withdrawn.
* **Not heap pressure** (`--Xmx` 1g/2g/4g/8g: no trend), **not the young-GC
  livelock**, **not the STW cross-thread takeover**
  (`CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200: no effect), **not the JIT-root path**
  (`--nojit` scales identically).
* **`jit_activation`'s global `Mutex` is gone** (per-thread tables since
  2026-07-31).
* **`Math.random()` is not a contention point** — a thread-local `Cell` seed
  (`native-builtins/src/lang_math.rs`), not a shared `Random`. Worth recording
  because `testConcurrentUpdate` calls it twice per update and a shared LCG is
  the obvious suspect.

## Measurement discipline this host requires

The first four are inherited; 5 and 6 are what the 2026-08-02 re-measurement had
to add after the old page's 4-thread arm turned out to be unresolvable.

1. **Never quote a debug-build ratio.** ~5-10x slower than release on its own.
2. **Never quote a multi-threaded wall-clock number.** 16 cores shared with
   15-40 sessions. Use CPU time, round-robin the arms, median or min of N, and
   record `uptime` beside every number.
3. **Aggregate `perf report` by symbol** (`--sort symbol`) — the default groups
   by command, which on a 25-thread run divides every symbol by 25 and puts
   nothing above 2.3 % — and use `--call-graph=dwarf`; the `fp` graphs resolve
   almost nothing above the leaf.
4. **An empty stdout is not a pass.** H2's `TestBase` reports some failures on
   stderr and the VM exits 1; check the exit code, not the output.
5. **Size the shape so the work term dominates the baseline.** VM start plus the
   `MERGE` seed is ~40 CPU-s with a ±15 CPU-s spread. A 4-thread × 200-update
   arm is 800 updates, about 3-6 CPU-s of work — subtracting that baseline from
   it measures the host. Use 10 000 updates at every thread count so the shapes
   are comparable to each other as well as resolvable.
6. **Interleave the 0-update baseline as an ordinary arm**, and pair the arms
   within a rep. Taken once up front, the baseline carries that minute's load
   into every number derived from it.

## Reproducing

```bash
javac -cp <h2>/target/classes -d probe H2UpdateScaleProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" \
  -Dprobe.dir=./h2updb H2UpdateScaleProbe <threads> <updates> 10000
```

Run `<threads> 0 10000` for the baseline of the same shape. The full class, when
you need the real thing (~20 min of CPU):

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home <jdk25> --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

## Related

* the retired `bug-h2-testmultithread-concurrent-update-timeout` write-up — this
  page's predecessor: the defects that are fixed, the corrected measurement, and
  what the old numbers got wrong.
* the retired `bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801`
  write-up — the INSERT half. Its flat ~25-30x across 1/2/4/8 threads is this
  page's constant factor, and its 4-thread arm was large enough to establish
  flatness where this page's was not.
* `bug-h2-classid0-stale-address-family.md` — the memory-safety family
  found in this class. Unrelated to throughput.
