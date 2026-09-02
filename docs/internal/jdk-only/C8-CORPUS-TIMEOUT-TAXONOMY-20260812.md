# C8 — `CV-TIMEOUT` was a wall-clock verdict on a shared host, and three different findings wore it

**Date:** 2026-08-12 **Lane:** C8 **Applies to:**
`regression-suite/corpus/run-corpus.sh`; every `CV-TIMEOUT` row already
recorded against a corpus.

---

## 1. The problem in one sentence

The driver's own comment said *"on this VM a timeout is VERY OFTEN a SIGSEGV
that produced no result line, not slowness"* — and then reported that suspicion
using **the same state, the same verdict and the same boilerplate note** as a
test that was merely slower than the cap, on a host whose wall clock this
project's own runner documents as non-metric.

Three findings with disjoint suspects were one word:

| what actually happened | who to talk to |
|---|---|
| the process **died of a signal** and printed no result line | the VM: a SIGSEGV/abort with no output |
| the process was **alive and silent** when the wall arrived | the VM: a hang, a livelock, a deadlock |
| the process was **alive and printing** when the wall arrived | nobody yet — this run does not know |

And a fourth thing wore it too: `137` was classified as `TIMEOUT`. Plain
`timeout` without `-k` never produces 137 (probed on this host: expiry is
**124**). A 137 is somebody else's `SIGKILL` — the OOM killer, a stray
`pkill` — and calling it a timeout attributes an external kill to slowness.

---

## 2. What separates them, and it is nearly free

**When did the arm last write to its log, relative to when it was killed?**

`stat -c %Y` on the arm's log versus the wall-clock second at which `timeout`
returned. A process still writing at the moment of the kill was making
progress. One that printed nothing for the last several minutes was not.

The signal is already in the stored evidence. `bc-java`'s
`crypto.hash2curve.test.AllTests`, in
`out/bc-java-jdk-only-20260812-220129/`:

```
cap                1500 s
killed at          1,509,793 ms   (rc=124)
oracle finished    112,531 ms     (rc=0, RAN)
cv.log last write  552 s BEFORE the kill
```

That is not a slow test. It ran silent for over nine minutes against an oracle
that finished the whole workload in under two, at ≥13x the oracle's wall. The
old harness printed the same sentence for it as for a suite that was still
emitting output when the timer fired.

---

## 3. What the driver does now

States (`cv_state`/`hs_state`), all of which used to be `TIMEOUT` or `CRASH`:

| state | trigger | note it produces |
|---|---|---|
| `SIGNAL` | rc = 128+N, or rc ≥ `0xC0000000` | *"died of signal=11(SIGSEGV) — a hard failure with a status, not a test result"* |
| `TIMEOUT-STALLED` | rc = 124, silent for ≥ max(30 s, cap/10) | *"killed at the Ns cap after Ms with NO output: SILENT AT THE WALL. That is a hang/stall, not slowness"* |
| `TIMEOUT-BUSY` | rc = 124, still writing | *"killed at the Ns cap while STILL WRITING output… This row does NOT separate 'hung' from 'slower than one cap' — it was tried at exactly one cap. Re-run with --timeout 3N before asserting either."* |
| `TIMEOUT-UNKNOWN` | rc = 124, mtime unavailable | *"hung vs slow is UNDETERMINED here"* |
| `LAUNCH-FAILED` | rc ∈ {125,126,127} and no `CORPUS-` marker | `HARNESS-ERROR`; never scored against the VM |
| `CRASH` | crash text in the log | unchanged, but its note now falls back to the decoded status instead of being empty |

Two new columns carry the evidence rather than a summary of it:

* **`cv_status` / `hs_status`** — the decoded exit status:
  `exit=0`, `timeout=124(expired-at-wall)`, `signal=11(SIGSEGV)`,
  `harness=127(command-not-found)`, `ntstatus=0xC0000005(ACCESS_VIOLATION)`.
  The raw `cv_rc` column is unchanged, so nothing that parsed it breaks.
* **`cv_silent_s` / `hs_silent_s`** — seconds between the arm's last output and
  its kill. This is the only column that separates hung from slow, and it is a
  measured interval, not a throughput number.

A `TIMEOUT-*` row against a `RAN` oracle also gets an **order-of-magnitude
bound**: *"Oracle finished the same workload in 112531 ms; CratonVM was killed
at >=13x that wall (ORDER-OF-MAGNITUDE BOUND, not a measurement)."* This is a
deliberate, narrow exception to the file's non-metric rule. A 13x ratio against
a same-host, same-workload reference run is a *shape* claim — it says "this is
not a 20% regression" — and it is worded on the row so it cannot be quoted as a
throughput figure. It is never emitted when the oracle did not itself reach
`RAN`.

---

## 4. What this does NOT do

**It does not retroactively classify the stored `CV-TIMEOUT` rows.** The
STALLED/BUSY split needs the kill time, which older runs never recorded. Those
rows stay unattributed, which is the honest state:

* 3 in `bc-java-jdk-only-20260812-213333`, 1 in `…-220129`
* 2 + 1 in the two `h2` runs
* 2 + 1 in the two `spring-framework` runs

Three CV timeouts cluster on arithmetic-heavy suites. **Two of them were only
ever tried at the 420 s cap, so they are not separated from the wall at all**,
and no shared cause may be asserted for the cluster. `TIMEOUT-BUSY`'s note now
makes that refusal a property of the row rather than something a reader has to
remember. This matches what `P4A-H2-DIVERGENCES-20260812.md` §3a/§3b already
declined to claim, and answers its §4 item 2, which asked in as many words for
*"a verdict of `CV-SLOW` distinct from `CV-TIMEOUT` when the arm is provably
still making progress"*.

**It does not make a timeout a measurement.** `cv_ms` is still non-metric and
the header still says so. `cv_silent_s` is an interval within one process's own
lifetime, and the oracle ratio is a bound, not a rate.

---

## 5. Exercised / not exercised

This lane could not run `cratonvm.exe`. Exercised on this host with stub
executables driving the real `cmd_run` against a real HotSpot oracle:

* `kill -SEGV $$` → `CV-SIGNAL`, `signal=11(SIGSEGV)`, exit 1
* `sleep 300` under a 40 s cap → `CV-TIMEOUT-STALLED`, *"after 41s with NO
  output"*, ratio `>=14x`
* a loop printing every 2 s under a 40 s cap → `CV-TIMEOUT-BUSY`, *"last write
  2s before the kill"*, plus the re-run-at-3x instruction
* bad-shebang stub → `LAUNCH-FAILED` → `HARNESS-ERROR`, exit 2
* all of the above also as `selfcheck` unit assertions, including
  `TIMEOUT-UNKNOWN` and the 137-is-not-a-timeout rule

**Not exercised:** a real Windows access violation from `cratonvm.exe`. The
`0xC0000005` mapping is unit-tested against the constant and taken from this
repo's existing records; it has not been observed by this lane.

---

## 6. How to read a `TIMEOUT-*` row from now on

1. `SIGNAL` — a hard failure with a status. Diagnose as a crash, not as a
   performance problem.
2. `TIMEOUT-STALLED` — the arm was alive and silent. Hang/livelock/deadlock, or
   a crash whose output never reached the log. **Do not** re-run at a bigger cap
   first; take a stack dump.
3. `TIMEOUT-BUSY` — this run does not know. Re-run at 3x the cap. If it then
   completes, it was slower than the cap and that is a throughput question this
   harness is not qualified to answer. If it stalls, it is (2).
4. Any of them, with the oracle at `RAN` — the ratio bounds how far apart the
   arms are, and nothing finer.
