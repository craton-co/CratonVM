# Two `vm/` probe caps were sized for a release binary, and the gate that runs them builds debug — FIXED

| | |
|---|---|
| **Status** | FIXED 2026-09-11 on `claude/cargo-test-workspace-20260911`. Filed the same day as OPEN; the fix is not the one the page proposed. |
| **Filed as** | `docs/known-issues/jit/vthread-and-prestart-probe-caps-are-sized-for-release-and-the-gate-runs-debug-20260911.md` |
| **Gate** | `cargo test --workspace` — `ci.yml` line 252, debug profile. |
| **Targets** | `vm/tests/vthread_probe_regression.rs`, `vm/tests/threadpoolexecutor_prestart_regression.rs` |

## What the page asked for, and what the measurements said

The page offered three ways out — scale the cap by profile, make the probes
cheaper, or run them against a release binary — and said each needed numbers
taken on an idle host first. It also named three things that were **not**
established and had to be settled before any cap moved. Those got settled, and
two of the three answers are not what the page expected.

### 1. "Is `VthreadProbe`'s 400 s run a heavy tail or a live spin?"

**Neither. It was the host.** The probe now prints a countdown, so the question
is answerable directly rather than by staring at a wall clock. Seven debug runs
on the same 8-core host at load 24-40:

| run | wall | user | outcome |
|---|---|---|---|
| 1 | 125 s | 25 s | `counted=10000 ok=true` |
| 2 | 52 s | 19 s | `counted=10000 ok=true` |
| 3 | 22 s | 21 s | `counted=10000 ok=true` |
| 4 | 179 s | 151 s | `counted=10000 ok=true` |
| 5 | 23 s | 19 s | `counted=10000 ok=true` |
| 6 | 74 s | 27 s | `counted=10000 ok=true` |
| 7 | 46 s | 26 s | `counted=10000 ok=true` |

Seven for seven correct, with an 8x spread in wall time and no spread at all in
the answer. The `user` column is the tell: runs 1, 5, 6 and 7 spent a quarter
to a half of their wall clock not running, which is a host with 25 other users
on it and not a VM defect. With the countdown in place the largest gap between
two consecutive advances across all of those runs was **7.8 s**.

### 2. "What IS `VthreadGcStress`'s debug time?"

Answered below. The original three 400 s+ kills were taken while the host was
at load 180-300; they are load, not profile.

### 3. "Does CI see it?"

Still unmeasured, and now it does not matter: the guard no longer depends on
how fast the host is. That is the point of the fix.

## `VthreadGcStress` in debug: the number, and what it forces

The filed page left this "unknown — three runs, three 400 s kills". It is not a
tail. Measured 2026-09-11, debug binary, one 8-core host, `remaining=` watched
throughout:

| threads x rounds | wall | user |
|---|---|---|
| 300 x 400 | 162 s | 141 s |
| 400 x 400 | 170 s | 161 s |
| 750 x 400 | 305 s | 283 s |
| 1500 x 400 | 319 s | 304 s |
| 1500 x 200 | 168 s / 170 s | 155 s / 156 s |
| **3000 x 400** (the gate's own size) | **killed at 2400 s**, 231 of 400 rounds done | — |

`user` tracks `real` in every row, so this is compute and not contention. Two
things fall out of the shape:

* There is a **~150 s floor** every row pays, which is 400 stop-the-world
  pauses at ~0.4 s each. A release pause is not 0.4 s; the whole release run is
  16-23 s.
* The per-pause cost grows with the number of **live continuations**, and at
  3000 it stops finishing.

That settles the question the page could not: `cargo test --workspace` as
`ci.yml` runs it — debug — cannot execute this target at its documented size,
and no stall budget, cap or ceiling changes that. It is not a guard problem at
all. **The workload had to be a decision**, and this is it:

* A **release** binary gets 3000 x 400, unchanged. That is the configuration
  the "8 hangs in 8 before the fix, 8 clean in 8 after" claim was measured at,
  and it is still the gate.
* A **debug** binary gets 1500 x 200 — 200 pauses against 1500 live
  continuations, the shape of the defect at a fifteenth of the thread-pause
  product, in 168 s.

Keyed on the profile of the **binary under test**, not `cfg!(debug_assertions)`:
the harness is not what runs for forty minutes, and both targets already prefer
`target/release/cratonvm` when one exists.

What that gives up is stated in the code: nobody has measured 1500 x 200
against a pre-fix binary, because the pre-fix binary is sixty commits back, so
the *deterministic* claim survives only in the release lane. A debug run that
detects the hang some of the time is worth having; a debug run nobody can
afford to wait for is not, and that is what was there.

## The 148-second stall, which is the answer to the page's first question

The page said a cap must not be raised until somebody explains
`VthreadProbe`'s middle run — "three runs cannot tell a heavy tail from a
live-spinning regression". With the countdown in place it can be explained,
and it is neither.

Four debug runs at load 33-42: 32 s, 41 s, 167 s, 167 s. The two long ones each
contain a **single stall of 146.9 s and 148.0 s** in the middle of the spawn
loop, and both then finish with `counted=10000 ok=true`. During the stall,
sampled live from `/proc`:

* `main-vm` is in state `R` and takes ~0.8 cores for the whole 148 s;
* one carrier is busy, the other seven are parked;
* `user` tracks `real` across the run as a whole — 136 s and 147 s of 167 s.

Four more runs on the same host at load 11-13 showed nothing like it: 28 s to
41 s, worst gap 12.5 s.

So it is **not** the 2026-09-05 deadlock, which consumed no CPU at all and
never finished. It is a long compute phase — an unoptimised collector or
compiler doing work the release build hides — and a Java-side progress signal
is structurally unable to report through it, because whatever holds the world
holds the thread that would print the heartbeat. That is why `VTHREAD_STALL` is
600 s and not 120 s: the budget has to clear the longest such phase, and 148 s
is the longest one measured.

## The fix: watch the counter, not the clock

A cap answers "has this taken too long?". Nothing in this suite wants that
answered — `VTHREAD_PROBE_CAP`'s own doc says, in italics, that it is a
**livelock guard, not a performance assertion**. What a livelock guard wants to
know is "is it still making progress?", and until now nothing in the probes
could tell anyone.

So the three probes print a counter that only goes down:

```text
progress remaining=5299 unspawned=2000 counted=6706 elapsedMs=39815
```

`remaining` is threads-not-yet-spawned plus threads-not-yet-finished, so it
falls monotonically from the first line to zero, including through the spawn
loop — which on a loaded debug run is most of the wall clock and used to be
invisible. `common::wait_watching` reads stdout a line at a time on its own
thread, and fails the test when `remaining` **stops falling** for
`VTHREAD_STALL`, not when a deadline expires.

The properties that buys:

* **A slow host cannot fail it.** The 179 s run and the 22 s run both advance
  their counter every few seconds. That is the failure mode this page was filed
  about, and it is gone by construction rather than by a bigger number.
* **A debug binary cannot fail it.** Same reason. The profile factor stops
  being something a constant has to be calibrated against.
* **A hang is caught FASTER than before, and reported better.** The 2026-09-05
  freeze produced no output from any thread; the stall clock starts at the last
  heartbeat before the freeze rather than at process start, and the panic says
  `since last advance=`, `advances=` and `lowest countdown=` instead of "timed
  out".
* **A live spin cannot hide in it.** Only a line whose value is *below the
  lowest seen so far* resets the clock. A probe that keeps printing while
  wedged is still caught — which is the objection the page raised against
  simply raising the cap, and the reason the key is a countdown rather than
  "any output".

The one thing it cannot see is a probe that advances for ever without
finishing. `VTHREAD_CEILING` (30 min, far above any measured run) is the
backstop, and its diagnosis says the counter *was* moving, which is the fact
worth having.

## Two defects found on the way, both fixed here

### The thread-state tripwire reported every virtual thread in the process

`thread_state::stress_checks_enabled()` defaults to `cfg!(debug_assertions)`,
so a debug build validates every recorded state transition against a table.
The table's model is **per OS thread**; the states it tracks belong to a
**logical** thread. Those are the same object only for platform threads. A
carrier multiplexes virtual threads: when one dies, `mark_dead` records
`Terminated` on the CARRIER's cell, and the carrier then picks up the next
continuation and records whatever that one is doing. The table had exactly one
edge out of `Terminated` — `Starting`, for a JNI thread re-attaching.

One `VthreadProbe` run, 10 000 virtual threads:

| count | edge | site |
|---|---|---|
| 8314 | `Terminated -> JavaRunning` | `gc_barrier::leave_blocked_region_flagged` |
| 1667 | `Terminated -> JavaRunning` | `thread_registry::mark_stw_ready` |
| 3 | `Terminated -> SafepointParked` | `leave_blocked_region_flagged:drain` |
| 2 | `Terminated -> SafepointParked` | `arrive_and_wait_inner:excluded` |
| 2 | `SafepointParked -> Terminated` | `arrive_and_wait_inner:resume` |

9 986 reports, one per virtual thread — **4.2 MB of `tracing::error!` on a
probe whose real output is 759 bytes**. And with the tripwire armed as designed
(`CRATONVM_STRESS_THREAD_STATES=1`, which makes violations fatal) all eight
carriers panicked and the VM wedged waiting for mutators that no longer
existed: the stress mode was unusable on any virtual-thread workload, which is
the workload it was most needed for.

`is_legal` now returns `true` for every edge out of `Terminated`, because a
cell in `Terminated` is a cell whose logical thread died and not an OS thread
that stopped. `try_record_transition` had always called `revive_current_cell()`
for `from == Terminated`; revival was already modelled, only the legality check
had not been told. After the change the same run reports **0** and its stderr
is 1 141 bytes.

This gives up one thing, stated in the code: a genuinely resurrected thread on
the same cell now reads as a revival. The record cannot tell them apart under
M:N, so the choice was between missing that and reporting every virtual thread
in the process.

**It is NOT why the debug runs were slow**, which is what it looked like. Three
paired runs came in at 46/179/46 s with the checks off against 22/23/74 s with
them on. Host load swamps it.

### The checked-in `.class` fixtures were never recompiled

`ensure_probes_compiled`'s contract says it compiles "if the classes directory
is missing **or stale** relative to the .java source files". The check was
`required.iter().all(|f| classes.join(f).exists())` — existence only, and
`vm/tests/resources/vthread_probe/classes/` is checked in. Editing a probe and
running the test re-ran the OLD class file and reported on source that was no
longer in the tree. Found by writing the heartbeats above, watching the test go
green, and seeing not one heartbeat in the output.

Same family as `common::warn_if_stale` one level down — that one catches a
launcher older than the sources, this one a fixture older than its own source —
and the same failure mode: a green that is about something other than what you
changed. It now compares mtimes.

## What changed

| file | change |
|---|---|
| `vm/tests/common/mod.rs` | `Progress` / `Stop` / `WatchedOutput` / `wait_watching` — wait on a child until it exits, stops advancing a countdown it prints, or hits an absolute ceiling. Drains both pipes throughout, as `wait_draining` does. |
| `vm/tests/vthread_probe_regression.rs` | `VTHREAD_PROBE_CAP` / `VTHREAD_GC_STRESS_CAP` replaced by `Guard` + `VTHREAD_STALL` / `VTHREAD_CEILING`; `gc_stress_workload()` sizes the stress probe by the profile of the binary under test; `ensure_probes_compiled` now compares mtimes instead of only testing existence. |
| `vm/tests/threadpoolexecutor_prestart_regression.rs` | 120 s deadline and its undrained `try_wait` poll loop replaced by `wait_watching`. |
| `vm/tests/resources/vthread_probe/VthreadProbe.java`, `VthreadGcStress.java`, `vm/tests/resources/cratonvm/ThreadPoolExecutorPrestartProbe.java` | print `progress remaining=…`, through the spawn loop as well as after it. |
| `vm/tests/resources/vthread_probe/classes/*.class` | recompiled — see the staleness defect below. |
| `vm/src/threading/thread_state.rs`, `docs/threading/thread-transition-states.md` | every edge out of `Terminated` is legal; the cell belongs to an OS thread and the state to a logical one. |

Verified on the branch, debug binary, `CRATONVM_REQUIRE_E2E=1`:

```text
test real_jdk_thread_pool_prestart_keeps_fresh_threads_startable ... ok
test result: ok. 1 passed; 0 failed;  finished in 42.47s

test vthread_counter_single_increments_to_1 ... ok
test vthread_tiny_builder_start_join ... ok
test vthread_probe_10000_all_increment ... ok
test vthread_gc_stress_completes ... ok
test result: ok. 4 passed; 0 failed;  finished in 196.38s
```

## What this does NOT do

* **It does not touch the thread counts.** 10 000 and 3 000 are the gate's
  content — `vthread_gc_stress_completes` is "the deterministic gate for the
  stop-the-world arrival hole", and 300 threads may not be. Option 2 on the
  filed page is declined.
* **It does not run these targets against a release binary.** Option 3 is
  declined for the reason the page gave: it would measure a different binary
  from the rest of the gate.
* **It does not scale anything by `cfg!(debug_assertions)`.** Option 1 is
  declined because it is unnecessary once the guard stops measuring time — and
  because it would have to be recalibrated every time the workload, the
  profile or the host changed, which is the treadmill this page is the second
  lap of.
* **It does not make debug builds faster.** They are 3-5x the release runtime
  here and the optimiser is most of that. Nothing in this gate now cares.
* **It does not convert the other 111 caps.** `vm/tests/*.rs` carries 114
  wall-clock timeouts of this shape, from 15 s to 600 s. Three are converted —
  the three that were failing. The rest are untouched and this page makes no
  claim about them.

## The general claim, for whoever hits the next one

A wall-clock cap can only express "took too long". Every one of those 114 was
introduced to express something else — "is it still making progress?", "did it
deadlock?" — and each one was calibrated against one profile, on one host, on
one day. When such a cap fires on a healthy run the repair everybody reaches
for is to raise it, and that is precisely the change that blinds it:
`VTHREAD_PROBE_CAP` went to 300 s and back on 2026-09-05,
`class_loader_unload_regression` sits at 600 s, and both were raised over runs
that were working.

A probe that reports its own progress does not need any of that, and the
budget it does need — how long a WORKING probe may go without advancing —
does not move when the profile, the host or the workload does.
`common::Progress` is written to be reused.

## Reproducing

```bash
JAVA_HOME=<jdk25> "$JAVA_HOME/bin/javac" --release 21 -d /tmp/vt \
  vm/tests/resources/vthread_probe/*.java
/usr/bin/time -f 'real=%e user=%U' \
  timeout -k 20 1800 target/debug/cratonvm -c /tmp/vt VthreadGcStress
```

Read `real` against `user`: a run whose `user` is a small fraction of `real`
was starved by the host, and one whose `user` tracks `real` was not. Read the
`progress remaining=` lines against each other: the largest gap between two
consecutive DECREASES is the quantity `VTHREAD_STALL` is a multiple of, and it
is the only quantity this gate now depends on.
