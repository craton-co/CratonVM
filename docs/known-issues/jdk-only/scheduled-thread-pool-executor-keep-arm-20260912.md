# Two `ScheduledThreadPoolExecutor` shadows are held by a keep arm, not by a defect

| | |
|---|---|
| **Status** | Open. Handed off from lane 5 on 2026-09-12; see §4 for what it needs. |
| **Came from** | [`lane-5-concurrent-thread-unsafe-RETIRED-20260910.md`](../../internal/retired/lane-5-concurrent-thread-unsafe-RETIRED-20260910.md) §4a, §9b.6, §11.5 |
| **Owner** | whoever owns the real-JDK Spring corpus — this is a corpus decision, not a lane one |
| **Instrument** | [`apps/probes/L5StpeKeepArm.java`](../../../apps/probes/L5StpeKeepArm.java) |

## 1. The two rows

```text
  java/util/concurrent/ScheduledThreadPoolExecutor
    <init>(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V
    getCorePoolSize()I
```

They are named by `keep_real_scheduled_executor_bridge` in
`native-api/src/registry.rs`, with a comment: kept for Spring's
`ThreadPoolTaskScheduler` anonymous subclass, *because the constructor delegates
to `ThreadPoolExecutor`'s real constructor and the getter resolves the inherited
field by name.*

## 2. Why they cannot simply be retired

They passed all four of lane 5's retirement preconditions and were in
`RETIRED_SHADOW_L5_TRIPLES` for most of a day. They came back out, and the
reason is structural rather than about these rows:

**A retirement table is MODE-BLIND and a keep arm is not.**
`NativeMethodRegistry::register` re-tags a retired triple `Bridge` →
`SyntheticStub` and *then* calls `register_inner`, where every
`keep_real_*_bridge` predicate opens with `effective_category() == Bridge`. By
the time the keep arm runs the answer is already `SyntheticStub`, so **a table
entry takes the native away in real-JDK mode too** — a mode no lane page asks
anyone to measure.

`registry::tests::real_layout_bridge_keeps_are_not_retired_shadows` walks every
retirement table and fails on exactly this, so the mistake cannot be made twice
silently. Lane 5 §9b.6 measured the same mechanism at scale: of 99 keep-listed
`ForkJoin` triples, 98 are outside what a retirement table can express.

So retiring these two means **deleting the keep arm and adding the table rows in
one change**. There is no intermediate step.

## 3. What has been measured, and what it does not settle

`apps/probes/L5StpeKeepArm.java` builds the shape the keep arm describes — an
anonymous subclass of `ScheduledThreadPoolExecutor` constructed through the
3-arg constructor — and exercises the constructor, the inherited getters,
`setCorePoolSize`, `submit`, `schedule`, and shutdown. Measured 2026-09-12:

```text
  HotSpot 25                                   7 rows
  CratonVM --jdk-only                          IDENTICAL to HotSpot
  CratonVM --jdk-only, dial armed on the class IDENTICAL again
    enforcement_dial   reached 23   yielded 23   declined_no_bytecode 0
    per triple         outcome = bytecode-won on BOTH of the two rows
```

**In isolation the keep arm is not needed.** That is the cheap half of the
evidence and it is not the half that matters:

* the probe has no Spring on the classpath, and the keep arm's claim is about
  Spring's subclass specifically;
* an armed dial and a retirement table are **not the same experiment** — the
  dial yields one dispatch and leaves the native registered for every other
  caller, while a table entry removes it. Lane 5 §9d.6 has two rows that passed
  a dial arm byte-identically and wrecked `ByteBuffer.allocateDirect` once built
  into a binary. Nothing here should be promoted on a dial arm alone.

## 4. What it needs

1. A real-JDK Spring corpus run on a binary built with the keep arm DELETED and
   the two triples in a retirement table — both halves, one binary. The
   `apps/spring-suite-runner/` harness on the build host is the vehicle.
2. Compare against the same corpus on the tree without the change, in the same
   sitting. A corpus number compared against one from a different binary is a
   number about that other binary.
3. If it holds: delete the arm, add the rows, and delete this page.
   If it does not: record WHICH vector fails and what it needs, and the arm
   stays with a measured reason instead of an inherited one.

## 5. Why this is not lane 5's

Lane 5's scope was `java.util.concurrent`, `Thread` and `Unsafe` shadows, and
it retired 223 of them. These two are not held by anything in that scope — they
are held by a judgement about a corpus, and the evidence that would change the
judgement comes from running that corpus. Leaving them on a retired lane page as
"what is left" made them look like unfinished retirement work, which they are
not.
