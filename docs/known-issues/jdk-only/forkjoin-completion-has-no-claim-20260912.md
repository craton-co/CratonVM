# A `ForkJoinTask` is completed by two models and claimed by neither

| | |
|---|---|
| **Status** | Open, and NOT a retirement item. The completion halves were made to agree on 2026-09-12; what is left is a contract change. |
| **Came from** | [`lane-5-concurrent-thread-unsafe-RETIRED-20260910.md`](../../internal/retired/lane-5-concurrent-thread-unsafe-RETIRED-20260910.md) §3a, §9b.6, §9g |
| **Instruments** | [`apps/probes/L5FjDouble.java`](../../../apps/probes/L5FjDouble.java), [`apps/probes/L5FjStatus.java`](../../../apps/probes/L5FjStatus.java) |
| **Not blocking** | shipped `--jdk-only` does not double. 0 of 20 rows, five runs out of five, on every binary measured. |

## 1. What is already done

`fjp_state` — the VM's side table — and `ForkJoinTask.status` — the word the
JDK's own bytecode reads — were two sources of truth that disagreed on every
shape. As of 2026-09-12 they agree at COMPLETION: `fjp_stamp_real_status` ORs
the bits the real `setDone()` / `trySetThrown()` / `trySetCancelled()` OR,
wherever the side table records a completion.

```text
  L5FjStatus, HotSpot 25 vs CratonVM --jdk-only
    before   8 rows differ, all of them statusDone true vs FALSE
    after    IDENTICAL
```

That also means a real worker's `doExec()` — whose first act is
`if ((s = status) >= 0)` — now declines to run a body this VM has already run.

## 2. What is left, and why it is a contract change

Agreeing at completion is not a claim. Measured on both binaries, twenty
submission shapes, five runs each:

```text
                      doubled / 20 rows
  hotspot             0/20
  dev    unarmed      0 0 0 0 0        <- shipped mode
  dev    pool armed   2 2 3 1 2
  trial  unarmed      0 0 0 0 0
  trial  pool armed   2 2 2 2 2
```

Unchanged, and the rows that double are the same ones. Both runners start
before either finishes, so a bit set at the END of the body cannot stop a body
already running.

**The only thing `doExec()` consults is `status < 0`.** So a claim this VM can
express means setting `DONE` *before* the body runs — which makes `isDone()`
answer `true` for a task that is still running, for every observer, in every
mode. That is a change to what the class promises, not a defect fix, and it is
why this is not a wave.

## 3. And why it cannot be approached as a retirement

`real_forkjoinpool` is ON by default in every mode, so the only ForkJoin
natives that exist are the ones `keep_real_forkjoinpool_bridge` and
`keep_real_forkjointask_bridge` name. Both predicates open with
`effective_category() == Bridge`, and the retirement re-tag in
`NativeMethodRegistry::register` has already made a retired triple a
`SyntheticStub` before they are consulted. Measured over the keep lists
themselves:

```text
  keep-listed ForkJoin triples          99
  kept as Bridge, DROPPED as stub       98   <- outside what a table expresses
  kept as both                           1   ForkJoinPool.execute(FJT)V
```

A table entry for any of those 98 does not make it yield in `--jdk-only`; it
deletes the native in every mode, compatible included. `registry::tests::
real_layout_bridge_keeps_are_not_retired_shadows` fails on exactly that.

## 4. The nearest neighbour, and why it matters here

`W3-4-forkjointask-status-flags-and-the-eager-default.md` is closed and is still
worth reading first. It documents `CRATONVM_FJP_EAGER_FORK`, a lever that
re-registers `fork()` on all three task classes so the body runs **at fork
time** rather than at `join()`.

That is the closest thing in the tree to a claim, and it is the obvious thing to
reach for. It is not one: running earlier changes WHEN the single inline runner
executes, and the double is two runners. But a taker should know the lever
exists, that it is genuinely wired flag-to-consumer, and that W3-4's own §3
condition (2) — the Spring/H2 A/B that would price it — has never been run.

## 5. What a taker needs

* read §3a of the lane page first — the double is a RACE, the count varies run
  to run, and one run of each configuration is not a measurement;
* both instruments exist and both have a recorded baseline, so the experiment is
  a re-run rather than a build-out;
* the decision to make is about `isDone()`'s contract during execution. Everything
  downstream of that decision is mechanical.

Whoever takes it owns the pool's completion model, not a list of rows.
