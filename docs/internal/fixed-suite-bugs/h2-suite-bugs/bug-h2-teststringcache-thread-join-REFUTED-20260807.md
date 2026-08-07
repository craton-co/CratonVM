# `TestStringCache` "`Thread.join()` blocked on dead threads" — REFUTED

## Status
**REFUTED / CLOSED 2026-08-07.** There is no `Thread.join()` defect. The
original reading was produced by a watchdog dump that printed a **stale**
frame chain for a thread that was *running*, with nothing marking it stale.

`TestStringCache.testMultiThreads()` — three worker threads and three
`Thread.join()`s — completes in **1.8–3.4 s** on CratonVM and **20/20**
consecutive `--nojit` runs finish clean. The class trips the suite's per-class
timeout for an entirely different reason, folded into
[`bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`](../../../known-issues/h2/bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md):
its `main()` runs a **benchmark** after the test, and that benchmark is ~85×
slower than HotSpot.

Two things came out of this and both have landed:

1. **The diagnostic is fixed** — `ThreadRegistry::dump_thread_summary_to_stderr`
   now tags every row with `deposit=live|STALE|post-mortem`. See
   [The fix](#the-fix).
2. **The class is reclassified** as a throughput-cliff instance in the
   perf-hang doc, with the twist that the slow part is not the test.

## What the original doc claimed

That all three workers were `alive=false` while `main` sat in
`java/lang/Thread.join@129`, and therefore either (1) a lost
termination-notify in `Thread.join()`, or (2) an untrustworthy `alive=` flag —
with reading 1 called "the more concerning" and worth a genuine
synchronization-bug hunt.

Both readings are wrong. So is the premise: `main` was not in `Thread.join()`
at all.

## What the dump actually said

The very same watchdog output contained a **live** stack dump, three lines
above the summary, and it did not agree with the summary:

```
--- T19.H1 stack dump: tid=0 name="main" frames=5 ---
tid=0 depth=0 class=org/h2/test/unit/TestStringCache method=main            pc=19
tid=0 depth=1 class=org/h2/test/unit/TestStringCache method=runBenchmark    pc=4
tid=0 depth=2 class=org/h2/test/unit/TestStringCache method=testToUpperCache pc=224
tid=0 depth=3 class=java/lang/String            method=toUpperCase          pc=28
tid=0 depth=4 class=java/lang/StringUTF16       method=toUpperCase          pc=228
```

`TestStringCache.main` is:

```java
public static void main(String... args) throws Exception {
    TestBase.createCaller().init().testFromMain();   // bci 0..15  — the TEST
    new TestStringCache().runBenchmark();            // bci 16..19 — the BENCHMARK
}
```

`main@19` is the second call. **`testFromMain()` had already returned** —
`testMultiThreads()`, its three `Thread.start()`s and its three
`Thread.join()`s all completed normally — and `main` had moved on to
`runBenchmark()`. It was `alive=true blocked=false`, i.e. executing bytecode,
and the printed `roots=12` / `top=Thread.join@129` were the snapshot it
deposited before its **last** blocking call, roughly 40 s earlier. Nothing in
the output said so.

Four consecutive reproductions all land in `runBenchmark → testToUpperCache`
at *different* bci (`testToUpperCache@126`, `@224`; `Character.toUpperCaseEx@3`,
`@29`; `StringUTF16.toUpperCase@58`, `@228`, `@237`) — the varying-offset
"progressing, just too slowly" signature the perf-hang doc uses, not a wait.

## Measurements

`org.h2.test.unit.TestStringCache`, Azure 16-core host, `--Xmx 1g`,
real-JDK 25 (`/home/victor/jdk25`), CratonVM @ `origin/dev` `1082eb446`:

| arm | the test only (`testFromMain()`) | the class's full `main()` |
|---|---|---|
| HotSpot 25 | **0.38 s** | **5.17 s** |
| CratonVM, JIT | **1.78 s** | **441.84 s**  (≈ **85×** HotSpot) |
| CratonVM, `--nojit` | **3.39 s** | **≫ 1 h** (see below) |

`runBenchmark()` is nine phases: three `testToUpperCache()` then six
`testSingleThread(100000)`. Under `--nojit` the *first* phase alone, measured
by the benchmark's own timers, is:

```
cache        20 900 ms      (HotSpot:   124 ms —   169×)
toUpperCase 1 194 295 ms    (HotSpot: 1 142 ms — 1 046×)
```

One phase of nine: **20 minutes**. Killed at that point; the class cannot
complete under any per-class timeout the suite could plausibly set.

The test half is 4.7–8.9× HotSpot — squarely the suite's ordinary CratonVM
overhead. The benchmark half is 85×, and that is the entire reason the class
reads as a hang: the suite runner invokes the class's `main`, so the benchmark
is unavoidable, and no per-class timeout the suite could reasonably set (300 s
today) would clear it.

**Negative control.** A driver that calls only
`new TestStringCache().init().testFromMain()` — the test, joins and all —
under `--nojit` with a 45 s watchdog armed:

```
TESTONLY_LOOP ok=20 bad=0
```

20/20. If a `Thread.join()` wakeup were being lost, this is the loop that
would catch it.

## Why the misreading happened, mechanically

Two registry fields are written on the way *into* a block and never cleared on
the way out:

* **`frame_trace` / `root_snapshot`** are deposited by
  `deposit_root_snapshot()` immediately before a thread blocks. Nothing clears
  them when it wakes. So for a **running** thread the chain is its last
  blocking call — arbitrarily old. `main`'s said `Thread.join@129` because the
  last thing it blocked on *was* a join, which had since returned.
* **`gc_block_state.in_blocked_region`** is raised by that same deposit. The
  thread-exit sequence in `vm_exec.rs`'s `thread_start` closure performs a
  final `deposit_root_snapshot()` before `mark_dead`, and has no counterpart to
  lower the flag — so **every dead thread reports `blocked=true` forever**.
  That is what made the three finished workers look like they were waiting on
  something.

The dead-thread flag is inert to the GC barrier: every census reads `alive`
first (`alive_count_blocked_and_os_tids_inner`, `blocked_os_tids`,
`fold_pointer_map_into_blocked`), so a dead entry never enters `expected`.
It was **only** ever a reporting artifact — which is precisely why the fix is
in the reporter and not in the barrier protocol. Adding a `store(false)` to
the exit sequence was considered and rejected: it would edit a GC-barrier
transition that is correct as written, to change a value nothing reads, and
it would not have helped the reader here anyway (a dead thread's *frame
chain* would still be its terminal empty deposit). Labelling is the
right-sized change.

This is the **third** investigation the untagged dump has sent the wrong way:

* `docs/internal/fixed-suite-bugs/springboot/onclasscondition-join-never-returns-20260801-FIXED.md`
  — read `alive=false` on retained dead entries as a missed join wakeup; 499
  clean runs later, it was stale entries from earlier test methods.
* `docs/internal/fixed-suite-bugs/elasticsearch-suite/ES-HANG-20260719-threadjoin-randomizedrunner-worker-windows-FIXED.md`
  — same `alive=false … blocked=true roots=1 top=<no-frame-trace>` shape.
* this doc.

The 2026-08-01 session added the full-frame-chain section to close the first
one, but left the chains **unlabelled**, which is the gap this capture fell
straight into.

## The termination-notify path was audited anyway — it is sound

The original doc's second "next step" was to grep the `Thread.join()` /
thread-termination-notify implementation for a race. Done, so nobody repeats
it. In real-JDK mode `Thread.join()` is **not** intercepted: `main` executes
the JDK's own bytecode, and `join(long)@129` is `invokevirtual
java/lang/Object.wait:(J)V` in the `millis == 0` branch
(`120: isAlive / 124: ifeq 135 / 129: wait(0) / 132: goto 120`). The VM side
that must wake it is the WP4.1 sequence at the tail of `thread_start`'s spawn
closure in `vm/src/vm/vm_exec.rs`:

```
wake_obj      = thread_registry.java_thread_obj(tid)      // GC-remapped, not the spawn-time ref
term_monitor  = monitors.enter_inflated_or_contend(wake_obj, tid)   // stable Arc<Monitor>
… flush SATB, deposit roots, enter_blocked …
finish_after(|| { clear_tlab_addr; mark_dead; release_monitors_held_by_except(tid, term_monitor) })
notify_all(tid); exit(tid)
```

Every hazard this shape has is already closed and commented in place: the
mirror address is re-read from the registry rather than reused from spawn (a
moving GC relocates it), the monitor is held as an `Arc` so the final
notify never re-looks-up through a raw address, and the blanket
dead-thread monitor sweep takes an `except` so it cannot force-release the
very monitor the notify needs to own — that last one is *precisely* this
"worker `alive=false`, joiner in `Thread.join()`" signature, and
`release_monitors_held_by_except`'s doc comment already says so. Nothing here
is broken; the report's shape came from the reporter, not the runtime.

## The fix

`vm/src/threading/thread_registry.rs` — `render_thread_summary()` (split out of
`dump_thread_summary_to_stderr()` so the format is unit-testable) now tags
every row via `ThreadRegistry::deposit_freshness(alive, blocked)`:

| tag | meaning |
|---|---|
| `deposit=live` | alive **and** inside a blocked region — `blocked=`/`roots=`/`top=` **are** its current state |
| `deposit=STALE` | alive and **running** — the chain is from its last blocking call; read the live stack dump instead |
| `deposit=post-mortem` | dead — `alive=false` is authoritative, the rest is termination residue |

A legend printed under the header spells all three out, the chain section
splits its count (`N at a live wait site, M STALE`), and each stale chain
carries its own inline banner pointing at the live dump. `debug_thread_census`
(the STW-barrier census) gets the same tag for the same reason.

The identical capture that produced this doc now reads:

```
--- T19.H1 thread summary: 4 registered thread(s) ---
  legend: deposit=live        — parked in a blocking native; blocked=/roots=/top= ARE its current state.
          deposit=STALE       — RUNNING bytecode right now. top=/roots= are whatever it deposited at its
                                LAST blocking call and can be arbitrarily old — read the live
                                "T19.H1 stack dump" above for where this thread actually is.
          deposit=post-mortem — dead. alive=false is authoritative; blocked=/roots=/top= are residue from
                                its termination sequence (nothing clears them), NOT evidence of a wait.
  tid=0 … name="main"     alive=true  … blocked=false roots=12 … deposit=STALE       top=java/lang/Thread.join@129 <- …
  tid=1 … name="Thread-0" alive=false … blocked=true  roots=1  … deposit=post-mortem top=<no-frame-trace>
  tid=2 … name="Thread-1" alive=false … blocked=true  roots=1  … deposit=post-mortem top=<no-frame-trace>
  tid=3 … name="Thread-2" alive=false … blocked=true  roots=1  … deposit=post-mortem top=<no-frame-trace>
--- T19.H1 full frame chains: 1 alive thread(s) with a deposited snapshot (0 at a live wait site, 1 STALE) ---
  tid=0 … name="main" (6 frame(s), oldest first) [deposit=STALE — thread is RUNNING (blocked=false); chain
  below is from its LAST blocking call and may be arbitrarily old. Its real position is in the
  "T19.H1 stack dump: tid=0" section above]:
```

`0 at a live wait site` is the sentence the original doc needed: **nothing in
this process is parked anywhere.**

Regression test:
`vm/src/threading/thread_registry.rs::tests::t19_summary_tags_a_running_threads_deposit_as_stale`
builds a registry with one running, one parked and one dead thread — all three
carrying the same `Thread.join@129` deposit — and asserts each row's tag, the
`(1 at a live wait site, 1 STALE)` split, the stale chain's banner, and the
legend.

## How to read `alive=` / `blocked=` from now on

The original doc worried that these columns "undermine every `alive=`/`blocked=`
read in every other watchdog dump in this sweep." The answer, now settled:

* **`alive=` is trustworthy.** It is the same flag `Thread.isAlive()` reads
  (`thread_is_alive` → `ThreadRegistry::is_alive`). `alive=false` means the
  thread's `run()` returned.
* **`blocked=` is trustworthy only while `alive=true`.** On a dead row it is
  termination residue.
* **The frame chain is trustworthy only when `alive=true && blocked=true`**
  — i.e. `deposit=live`. Otherwise it is history.
* When the summary and the live `T19.H1 stack dump` disagree about the same
  tid, **the live dump wins**. The summary is a deposit; the dump is a read of
  the actual frames.

## Also observed en route (not chased here)

The JIT arm's 441 s run logged eight
`[moving-young] fallback #N: reason=innermost-rbp-belongs-to-unguarded-callee`
warnings — the young generation degrading to the non-moving sweep because a
live JIT frame could not prove a complete rewritable root map. Unrelated to
this doc's question; noted for whoever owns the moving-young fallback cluster.

## Related
* [`bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`](../../../known-issues/h2/bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md)
  — where this class belongs; it is listed there as a confirmed instance.
* `docs/internal/fixed-suite-bugs/springboot/onclasscondition-join-never-returns-20260801-FIXED.md`
* `docs/internal/fixed-suite-bugs/elasticsearch-suite/ES-HANG-20260719-threadjoin-randomizedrunner-worker-windows-FIXED.md`
