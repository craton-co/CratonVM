# Two `vm/` probe caps are sized for a release binary, and the gate that runs them builds debug

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-11 at `origin/dev` = `8f1666414`. Measured, not fixed: the repair is a decision about the caps, not a defect to correct. |
| **Gate** | `cargo test --workspace` — `ci.yml` line 252, debug profile. |
| **Targets** | `vm/tests/vthread_probe_regression.rs`, `vm/tests/threadpoolexecutor_prestart_regression.rs` |
| **Found by** | `claude/cargo-test-workspace-20260911`, after turning `registrar_drift` green let a fail-fast gate reach `vm/` for the first time in weeks. |

## Why nobody has seen this

`cargo test --workspace` stops at the first failing test binary.
`registrar_drift` is a `native-builtins` target and had been red on `dev` since
`0b2791ac7`, so **every `vm` target ran after it and none of them ran at all**.
That is the thesis of
[`cargo-test-workspace-is-red-on-dev-tip-in-seven-places-FIXED-20260911.md`](../internal/retired/cargo-test-workspace-is-red-on-dev-tip-in-seven-places-FIXED-20260911.md),
and this page is what it turned up.

## The measurement

One 8-core Linux host, load 18-41, `origin/dev` sources, one binary of each
profile, probes driven directly with no test harness involved.

| probe | release | debug |
|---|---|---|
| `VthreadProbe` (10 000 vthreads) | 6.3 s | 30.1 s, **400 s+**, 31.8 s |
| `VthreadGcStress` (3 000 vthreads + churn) | 23.4 / 17.1 / 16.0 s | **400 s+**, **400 s+**, **400 s+** |
| `ThreadPoolExecutorPrestartProbe` (2 000 workers) | — | 147 s, `PRESTART_OK iterations=2000 workers=2000` |

The caps they are run under:

```rust
const VTHREAD_PROBE_CAP: Duration = Duration::from_secs(60);
const VTHREAD_GC_STRESS_CAP: Duration = Duration::from_secs(120);
// threadpoolexecutor_prestart_regression.rs
let deadline = Instant::now() + Duration::from_secs(120);
```

`400 s+` means `timeout 400` killed it, so the debug figure for `VthreadGcStress`
is a floor and not a time. Every completed run printed `OK`, and the prestart
probe prints its success line at 147 s against a 120 s deadline: **the assertions
are satisfied and the clock is what fails.**

## Where the caps came from

Both `vthread` constants document their own derivation, and both derivations are
release measurements:

> `VTHREAD_PROBE_CAP` … 60 s — twenty times the healthy runtime and twice the
> worst completed run ever measured

with the healthy runtime recorded as "8 runs 2-4 s" taken by "running the VM
DIRECTLY, no test harness involved" — a release binary. In debug the healthy
runtime is ~30 s, so 60 s is 2x the healthy runtime, not 20x, and the constant no
longer means what its comment says.

> `VTHREAD_GC_STRESS_CAP` … Healthy runs finish in 7-22 s on a loaded 8-core host

which the release column above reproduces exactly (16-23 s). The debug column
does not finish in more than 17x that.

A third target had the identical mistake and is FIXED rather than filed:
`class_loader_unload_regression` ran a fixed 182-collection schedule that cost
15.7 s in release and over 400 s in debug. That one had a repair that is not a
cap change — the probe now polls and needs 8 collections — so it was made. See
`edc3a51fa`.

## The one reading that is NOT available

"The host is busy." It is a shared box and it was, but the release runs were
taken in the same window at the same load and finish in 16-23 s. The variance is
between PROFILES, not between load levels. The `-O`-less build of a collector and
a virtual-thread scheduler is 15-25x slower here, which is unremarkable in
itself; what is not is a wall-clock cap that was only ever measured on one side
of that factor.

## What is NOT established, and why this is filed rather than fixed

* **`VthreadProbe`'s middle run.** 30 s, then 400 s+ with 155 s of user time,
  then 31.8 s. A process burning CPU for 400 s is not the deadlock
  `vthread-probe-intermittent-hang-FIXED-20260905.md` describes (that one used
  no CPU at all), but three runs cannot tell a heavy tail from a live-spinning
  regression, and this host at load 29-41 cannot settle it. **Anyone raising the
  cap must answer this first**, because a cap raised over a live-spin is exactly
  the change that hides it.
* **`VthreadGcStress`'s debug time.** Unknown — three runs, three 400 s kills.
  Sizing a cap needs the number, which needs an idle host.
* **Whether CI sees it.** `ubuntu-latest` is 2-4 cores and idle; slower per core
  than this host and far less contended. The prestart probe's 147 s at 28 s of
  user time says contention, not compute, dominates there — so CI may be fine.
  Nobody has measured it, because the fail-fast gate has never reached these
  targets on CI either.

## The three ways out, none of them free

1. **Scale the cap by profile.** `cfg!(debug_assertions)` times a measured
   factor, so the constant keeps meaning "N times the healthy runtime in THIS
   profile". Honest, and it needs the numbers above taken on an idle host first.
   It also makes a real hang cost 20x longer to report in debug, which is the
   objection that took `VTHREAD_PROBE_CAP` back from 300 s to 60 s on 2026-09-05.
2. **Make the probes cheaper.** What was done for `class_loader_unload`. Here it
   means fewer virtual threads, and the counts (10 000 and 3 000) are the gate's
   content — `vthread_gc_stress_completes` is described as "the deterministic
   gate for the stop-the-world" defect, and 300 threads may not be.
3. **Run these targets against a release binary.** Both already prefer
   `target/release/cratonvm` when it exists; CI builds only debug. Changing that
   is a CI decision and would measure a different binary from the rest of the
   gate.

## Reproducing

```bash
JAVA_HOME=<jdk25> "$JAVA_HOME/bin/javac" --release 21 -d /tmp/vt \
  vm/tests/resources/vthread_probe/*.java
/usr/bin/time -f 'real=%e user=%U' \
  timeout 400 target/debug/cratonvm -c /tmp/vt VthreadGcStress
/usr/bin/time -f 'real=%e user=%U' \
  timeout 400 target/release/cratonvm -c /tmp/vt VthreadGcStress
```

Read `real` against `user`: a run whose `user` is a small fraction of `real` was
starved by the host, and one whose `user` tracks `real` was not.
