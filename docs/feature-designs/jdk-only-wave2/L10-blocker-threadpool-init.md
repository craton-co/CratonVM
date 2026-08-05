# L10 — Blocker: real `ThreadPoolExecutor` field initialisation

**Owns:** `native-collections/src/lib.rs` (whole file)
**Gated on:** nothing — but **it gates L11**.
**Conflicts:** L2 owns the same file. **L2 lands first**; it is smaller.
**Effort:** L
**Note:** not a jdk-only change. Same caveat as L9 — staff it as a VM defect.
**Evidence:** [`threadpoolexecutor-execute-receiver-shape-special-case-copies.md`](../../known-issues/jdk-only/threadpoolexecutor-execute-receiver-shape-special-case-copies.md)

## Goal

`Executors.new*ThreadPool()` must return objects built by the **real**
`ThreadPoolExecutor.<init>`, with real field initialisation. Until then,
reclassifying `native_es_execute` drops it under `--jdk-only` and **strict mode
loses thread pools**.

That is what makes it a blocker rather than cleanup: the eight
receiver-shape dispatch sites in L11 exist precisely to detect "this executor
was not built by real bytecode" and route around it.

## Current state

The marker undercount that made item 7 tier-1 is gone: a census constant names
all eight sites plus the ninth unconditional `force_native` arm they exist to
override, the receiver-shape probe has **one** implementation instead of three,
and a gate fails on a partial sweep. **No site is deleted** — that needs this
lane first.

## Steps

1. Establish what real `ThreadPoolExecutor.<init>` needs that it does not get.
   The `ctl` `AtomicInteger` is the known one — a synthetic executor NPEs
   immediately on it (see `fixed-suite-bugs/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`).
   Expect `mainLock`, `workers`, `workQueue` to be in the same family.
2. Make `Executors.new*ThreadPool()` run the real constructor chain rather than
   allocating a synthetic shape.
3. Confirm the receiver-shape probe (`threadpool_executor_has_real_workers`)
   returns `true` for every executor the factories produce. That predicate is
   the one all eight sites consult; when it is universally true, the sites are
   dead and L11 can delete them.

## Verification

* `JdkOnlyCensusLoadProbe`'s `concurrent` section under `--jdk-only`: fixed and
  cached pools, 24 submitted tasks, a latch, and bounded `Future.get`. It must
  match HotSpot exactly. This section was **dead** before the thread-start fix,
  so it is a genuine new signal.
* Both probes vs HotSpot, both modes, exit status checked.
* `cargo test --release -p cratonvm-native-collections --lib` (94 tests).
* The eight-site census gate must still pass — this lane does not delete sites,
  it makes deleting them possible.

## Watch for

Executors are where the **socket/executor hang** shows up
([bounded socket operations hang about one run in five](../../known-issues/bounded-socket-operations-hang-about-one-run-in-five.md)).
That hang is pre-existing and mode-independent; do not attribute it to this
lane's changes without an A/B against the pre-fix binary and a HotSpot control.

## Done when

`Executors.new*ThreadPool()` returns real-constructed executors,
`threadpool_executor_has_real_workers` is true for all of them, and L11 is
unblocked.
