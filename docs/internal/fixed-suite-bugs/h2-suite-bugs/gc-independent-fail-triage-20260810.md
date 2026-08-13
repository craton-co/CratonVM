# The H2 FAILs that are collector-independent — triaged to 3 classes, one family

**Status: TRIAGED, 2026-08-10.** Follow-on from
`gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md`, which fixed that
sweep's crashes and left its FAILs partly attributed against a HotSpot control
taken on a **loaded** host. This page redoes that control idle, over exactly the
classes it matters for, and answers the question the previous page could not:
*which H2 FAILs are CratonVM's, independent of the collector?*

**Three.** `TestTransaction`, `TestWeb`, `TestBnf`. All three are the same
family — CratonVM missing a hardcoded millisecond budget baked into the H2
fixture — and none of them is a correctness defect in H2, in the collectors, or
in dispatch.

## Method

Binary: `dev@60d50f4c6` (post the NIO view-storage fix, pre the direct-call
deopt fix). The sweep's 62-class non-passing union, run under **Generational,
G1 and ZGC from one binary** selecting the collector by runtime flag, 300 s per
class, `--Xmx 1g`:

| collector | PASS | FAIL | CRASH | HANG |
|---|---:|---:|---:|---:|
| Generational | 19 | 17 | 0 | 29 |
| G1 | 19 | 16 | 5 | 25 |
| ZGC | 20 | 24 | 0 | 21 |

Intersecting the three FAIL sets gives **13 collector-independent FAILs**. Those
13 then went to a stock HotSpot 25 control **with the host to itself**
(`load1` 2.9 — the earlier control ran at ~20 and that mattered, see below).

## Result

| class | CratonVM (all 3 collectors) | idle HotSpot 25 | verdict |
|---|---|---|---|
| `db.TestTransaction` | FAIL `Expected: 100 actual: 50` | **PASS** | **CratonVM** |
| `server.TestWeb` | FAIL `does not contain: '` | **PASS** | **CratonVM** |
| `unit.TestBnf` | FAIL `Expected: true got: false` | **PASS** | **CratonVM** |
| `db.TestFunctions` | FAIL `AssertionError: Failure` | FAIL, same | H2/JDK 25 |
| `poweroff.TestRecoverKillLoop` | FAIL `error! renaming file` | FAIL, same | harness shape |
| `synth.TestJoin` | FAIL Postgres connect | FAIL, same | environment |
| `synth.TestTimer` | FAIL `NULL not allowed for column` | FAIL, same | H2 |
| `synth.sql.TestSynth` | FAIL NPE in the fuzzer | HANG | H2 fuzzer, nondeterministic |
| `synth.thread.TestMulti` | FAIL SQL syntax on `VALUE` | FAIL, same | H2 fixture |
| `unit.TestClassLoaderLeak` | FAIL `AppClassLoader` cast | FAIL, same | JDK 9+ |
| `unit.TestExit` | FAIL (non-zero exit) | FAIL, same | harness shape |
| `unit.TestMemoryUnmapper` | FAIL `Expected: 2 actual: 1` | FAIL, same | H2/JDK 25 |
| `unit.TestTools` | FAIL `Connection is broken` | FAIL, same | H2 |

**10 of 13 fail identically on stock HotSpot.** They are not CratonVM defects
and should not be re-triaged; the five of them this project had already filed
that way are in
`bug-h2-suite-fail-cluster-not-cratonvm-bugs-20260807-NOT-A-BUG.md`.

### Read this before reusing the earlier control

The 2026-08-10 control in the sibling page ran while three CratonVM suites
shared the host. On that run **HotSpot failed `TestWeb`**; idle, it passes.
That one row is the difference between "H2's autocomplete budget is
unreachable for everyone" and "CratonVM misses it" — and it is the whole reason
this page re-ran the control. Any measurement of the three CratonVM classes
below has to state host load, because all three sit within tens of milliseconds
of their budget.

## The three, and why they are one family

Each fails against a hardcoded millisecond budget in the fixture, not against a
wrong answer.

### `TestTransaction.testMergeUsing` — H2's own `TestAll.lockTimeout = 50` ms

Two connections race a 50-statement `MERGE` batch for one table lock. Batch
time, min of 5:

| arm | min ms |
|---|---:|
| HotSpot 25 | **22** |
| CratonVM, JIT threshold 500 (default) | 108 |
| CratonVM, `--nojit` | 92 |
| CratonVM, JIT threshold 500 000 | **87** |

The loser's whole batch dies with `Timeout trying to lock table "TEST"`, the
test swallows it (`catch (SQLException e) { // Ignore }`) and asserts
`Expected: 100 actual: 50`. **This one IS an instance of
`../../../known-issues/vm/jit-net-negative-on-call-dense-classes-20260810.md`** —
it reproduces that page's exact arm ordering (default-threshold JIT worst,
JIT-with-almost-nothing-compiled best, beating `--nojit` too) on a workload
sharing nothing with `ZipContentTests`. The lever buys 20% and does not
recover it: 87 ms is still 1.7x the budget.

Ruled out on the way, each by a measurement: not a JIT miscompile (`--nojit`
reproduces the FAIL identically), not a compile-gate defect
(`CRATONVM_DBG=jit-method-stats`: `hot_but_stuck_in_interpreter=0`,
`c2_bailouts=0`, `refused=0`), not a lock-handling defect (at
`LOCK_TIMEOUT=500` both connections pass).

### `TestBnf` + `TestWeb` — H2's `Sentence.MAX_PROCESSING_TIME = 100` ms

Measured per query (see the follow-up on
`../../../known-issues/h2/!bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`,
which owns this pair): grammar setup is **18.6x** (69 ms → 1282 ms), the
**first, cold** query is 230 ms and is the only one that misses, and every warm
query lands at 50-107 ms against a 100 ms budget. The JIT threshold is **inert**
here — 222 / 206 / 252 ms across threshold 500 / 500 000 / `--nojit` — so unlike
`TestTransaction` this is raw interpreter throughput, not the admission policy.

The useful consequence: the remaining distance is **~1.3-2x and concentrated**,
not the open-ended programme that page's 2026-07-23 follow-up concluded with.

## What is deliberately not done here

No fix. All three need throughput work that belongs to
`jit-net-negative-on-call-dense-classes` (for `TestTransaction`) and to the BNF
page's own interpreter-throughput residual (for the other two), and neither is
a scoped patch. What this page contributes is the scoping: **the collector is
irrelevant, 10 of the 13 are not ours, and the 3 that are share one shape and
one measured distance.**

## Related

- `gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` — the sweep this
  came from, and the loaded-host control this page supersedes for these classes.
- `bug-h2-suite-fail-cluster-not-cratonvm-bugs-20260807-NOT-A-BUG.md`
- `../../../known-issues/vm/jit-net-negative-on-call-dense-classes-20260810.md`
- `../../../known-issues/h2/!bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`
