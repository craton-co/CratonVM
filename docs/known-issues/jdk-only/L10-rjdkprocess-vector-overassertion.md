# `RJdkProcess` was a broken oracle — it asserted a process-tree snapshot against a child that had already exited

**Status:** FIXED in the vector (2026-08-06). Lane L10 of the jdk-wave2 pool.
**No VM code was changed. There is no CratonVM bug here to fix** — the test was
wrong, and every CratonVM `RJdkProcess` result recorded before this date was
measured against an oracle that real HotSpot 25 also fails.

## The failure

`regression-suite/src/RJdkProcess.java` was counted as a CratonVM failure. Real
HotSpot 25 (`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`, no CratonVM
involved) fails it too, and fails it *intermittently* — which is the tell:

```
$ cd regression-suite
$ java -cp build RJdkProcess          # HotSpot 25.0.3+9-LTS, 3 consecutive runs
=== run 1 ===
CK RJdkProcess self alive=true pidPositive=true infoNonNull=true parentOptionalNonNull=true
Exception in thread "main" java.lang.AssertionError: the child must appear in our children()
	at RJdkProcess.check(RJdkProcess.java:33)
	at RJdkProcess.childProcess(RJdkProcess.java:132)
	at RJdkProcess.main(RJdkProcess.java:204)
rc=1
=== run 2 ===
... AssertionError: the child must appear in our children()      (same, line 132)
rc=1
=== run 3 ===
CK RJdkProcess self alive=true pidPositive=true infoNonNull=true parentOptionalNonNull=true
Exception in thread "main" java.lang.AssertionError: the child must appear in our descendants()
	at RJdkProcess.check(RJdkProcess.java:33)
	at RJdkProcess.childProcess(RJdkProcess.java:134)
	at RJdkProcess.main(RJdkProcess.java:204)
rc=1
```

Run 3 got one line further than runs 1-2. The same binary, the same JDK, a
different answer — the assertion was a race, not a contract.

## What the test asserted

```java
Process p = pb.start();                     // cmd.exe /c exit 3   (or sh -c 'exit 3')
ProcessHandle h = p.toHandle();
...
check(h.parent().isPresent(), "a freshly forked child must report a parent");
check(ProcessHandle.current().children().anyMatch(c -> c.pid() == h.pid()),
        "the child must appear in our children()");
check(ProcessHandle.current().descendants().anyMatch(c -> c.pid() == h.pid()),
        "the child must appear in our descendants()");
```

The subject is a child chosen precisely *because it exits immediately* (it is
the exit-code vector). The test then asks three questions that only have an
answer while the child is still running.

## Root cause

`ProcessHandle.parent()`, `.children()` and `.descendants()` are documented as
**snapshots of the live OS process table taken at call time**, explicitly racy
by contract. A process that has terminated is not in that table any more:

* on Windows it leaves the snapshot the moment it terminates;
* on Unix it leaves as soon as the JDK's process-reaper thread `wait()`s on it,
  which happens promptly and asynchronously.

`cmd.exe /c exit 3` lives for single-digit milliseconds. Whether it is still in
the table when `children()` is called is a scheduling coin-flip, and each of the
three queries is a *separate* snapshot, so the coin is flipped three times.

Measured directly (`Probe.java`, HotSpot 25, this host, 5 trials each):

```
# long-lived child  (cmd.exe /c ping -n 30 127.0.0.1)  -- 5/5 identical
trial=0 alive=true inChildren=true inDesc=true iters=0 ms=63 parentPresent=true descCount=3 childCount=1
trial=1 alive=true inChildren=true inDesc=true iters=0 ms=56 parentPresent=true descCount=3 childCount=1
... (trials 2,3,4 identical)

# fast-exiting child (cmd.exe /c exit 3)               -- 5/5 identical
fast trial=0 alive=true parentPresent=true inChildren=true  inDesc=false
   after waitFor: parentPresent=false inChildren=false
... (trials 1-4 identical)
```

Two facts settle it:

1. A **live** child is in `children()` and `descendants()` on the *very first*
   snapshot, every time (`iters=0`). There is no fork-visibility delay to
   tolerate; the query is not slow or lossy.
2. A **fast-exiting** child is already missing from `descendants()` by the time
   that second snapshot is taken, a millisecond after `children()` saw it — and
   after `waitFor()` even `parent()` reports empty. The child is simply gone.

So the platform never promised what the test demanded. `descendants()` vs
`children()` was *not* the issue (the child is a direct child under both
`cmd.exe /c` and `sh -c`); liveness was.

## The fix

`childProcess()` is split into two parts that no longer share a subject:

* **Part 1 — process-tree relationships**, asserted against the *sleeper*
  (`ping -n 30` / `sleep 30`), which is guaranteed alive for the whole window.
  `parent()`, `children()`, `descendants()` and `of(pid)` are all asked here.
* **Part 2 — exit-code plumbing** (`waitFor`, `exitValue()==3`, `onExit()`),
  asserted against the exit-3 child, which asks nothing about the process tree
  and is therefore free to die as fast as it likes.

A new helper polls the snapshot rather than sampling it once:

```java
static boolean awaitInTree(long pid, boolean descendants) throws InterruptedException
```

bounded by `TREE_WAIT_MS = 10_000`, deliberately far below the sleeper's own 30s
lifetime — so a timeout can only mean "the tree query is broken", never "the
child had already exited". Two guards make that reasoning checkable rather than
assumed: `lh.isAlive()` before the tree section and `live.isAlive()` after it.

**Nothing was weakened or deleted.** All 18 original `childProcess` assertions
survive; the three racy ones were re-pointed at a subject for which the platform
actually makes the guarantee. Seven assertions were *added*:

* `lh.isAlive()` before the tree section and `live.isAlive()` after it, so the
  section's own precondition is proved rather than assumed;
* `!lh.isAlive()` after `destroyForcibly`, so the *handle* — not just the
  `Process` — is required to report the death;
* `of(pid).isPresent()` and `of(pid).equals(handle)` for a child (previously
  only exercised for the current process);
* "pid agrees with its handle" and "the child is a different process" are now
  asserted for *both* children instead of one.

Check count went from 46 to 53 (53 is measured; 46 is a static count, since the
old vector never reached its own total).

## Verification

`javac -d build src/RJdkProcess.java` then 8 serial runs, all identical:

```
CK RJdkProcess self alive=true pidPositive=true infoNonNull=true parentOptionalNonNull=true
CK RJdkProcess child exit=3 parentIsUs=true killedThenDead=true
CK RJdkProcess covered=[allProcesses, children, current, descendants, destroyForcibly, info, isAlive, of, onExit, parent]
CK RJdkProcess checks=53
PASS RJdkProcess (53 checks)
rc=0
```

Then 18 more under self-inflicted load (3 rounds x 6 concurrent JVMs), since
host load is where the old race bit hardest — 18/18 `rc=0 :: PASS RJdkProcess
(53 checks)`. **26/26 green, 0 flakes.** The pre-fix vector failed 3/3.

## Deliberately left alone

* `check(!live.waitFor(50, TimeUnit.MILLISECONDS), "timed waitFor must time
  out")` — wall-clock-shaped, but the subject sleeps 30s and its liveness is
  asserted on both sides. Not the same defect class.
* `long visible = ProcessHandle.allProcesses().limit(4).count(); check(visible
  >= 0, ...)` — **vacuous**: `count()` is never negative, so this assertion
  cannot fail. It still exercises the stream, and tightening it (e.g. "the
  current process must be visible") would import exactly the kind of
  platform assumption this record is about, since `allProcesses()` is
  permitted to be restricted. Under-assertion, noted not changed.
* `ProcessHandle.of(Long.MAX_VALUE).isEmpty()` — safe on both platforms
  (Windows pid is a `DWORD`, Linux `pid_t` an `int`).
* Every `IllegalStateException` / `IllegalThreadStateException` /
  `IOException` expectation — documented contracts, deterministic, and each
  would legitimately catch a VM bug.

## What this means for the CratonVM result

Any `RJdkProcess` failure recorded before this fix must be **re-measured**. A
pre-fix failure at `RJdkProcess.java:132` or `:134` carries no information about
CratonVM: HotSpot fails there too. Post-fix, a failure in the tree section is
real — the child is provably alive for the full 10s window, so a CratonVM
`children()`/`descendants()`/`parent()` that comes up empty is a genuine defect
in the `--jdk-only` `ProcessHandle` bridges.
