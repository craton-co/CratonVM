# HIB-LONGTAIL.1 (`org/h2/`) — re-verified 2026-07-28, ban STAYS

**Status**: re-tested against the 2026-07-27 atomic-array RMW fix
(`ffb8dfa22`, already on `dev`). That fix did NOT clear this ban's
residuals. Three genuine JIT-caused regressions confirmed via A/B; a
fourth failure (`TestFileSystem` hang) was ruled OUT as unrelated to this
ban. **`org/h2/` in `vm/src/jit/skip_list.rs` (`HIB-LONGTAIL.1`) stays
banned.**

## Why this re-test happened

`docs/known-issues/h2/h2-jitban-residuals-20260726.md` recorded this
ban's sharpest blocker as a CORRECTNESS bug: `TestFileSystem`'s
`memLZF:`/`nioMemLZF:` `testConcurrent` intermittently read a stale
`expected` value against fresh file bytes, root-caused to
`AtomicIntegerArray`/`AtomicLongArray` RMW natives not actually being
atomic (`compareAndSet` was a bare read+write with no exclusion). That
was fixed the very next day in `ffb8dfa22` (`fix(hibernate): repair
criteria and concurrent runtime paths`, 2026-07-27) — see
`native-builtins/src/util_concurrent_ext.rs`'s `atomic_array_rmw`/
`atomic_array_cas`, now routed through `compare_and_swap_field`'s
per-object CAS lock. The fix was never re-tested against this specific
ban afterward. This re-test closes that gap.

## Re-test setup

Binary: fresh release build off `origin/dev` at `072f3de9c` (includes
`ffb8dfa22`), `/data/tmp/cratonvm-h2longtail1-20260728`.

`RAtomicArray` (the regression-suite gate for the exact defect this ban
used to attribute to JIT reordering) passes on this binary:

```bash
CV=/data/tmp/cratonvm-h2longtail1-20260728 \
  JDK=/data/jdk25-real-20260717/jdk-25.0.3+9 \
  ONLY=RAtomicArray bash regression-suite/run.sh
# RAtomicArray   PASS
```

Full 17-class regression suite: 17 passed, 0 failed on the same binary.

## The four classes from the 2026-07-26 doc's residual-4, re-run both ways

`CRATONVM_JIT_ALLOW_PACKAGES=org/h2/` (ban LIFTED) vs. default (ban
ACTIVE), same binary, run individually (not concurrently — this host's
runs interfere when run 3-at-once, per the 2026-07-26 doc):

| Class | Ban ACTIVE (control) | Ban LIFTED |
|---|---|---|
| `TestFreeSpace` | (not re-run; already closed 2026-07-27) | PASS 83.0s |
| `TestStreamStore` | (not re-run; already closed 2026-07-27) | PASS 6.7s |
| `TestNestedJoins` | (not re-run; already closed 2026-07-27) | PASS 67.5s |
| `TestRunscript` | **PASS** 191.3s | **CRASH** 11.3s |
| `TestPageStoreCoverage` | **PASS** 7.1s | **CRASH** 4.3s |
| `TestReopen` | **PASS** 7.0s | **CRASH** 4.2s |
| `TestFileSystem` | **HANG** 300.1s (timeout) | **HANG** 300.1s (timeout) |

Three classes (`TestRunscript`, `TestPageStoreCoverage`, `TestReopen`)
PASS with the ban active and CRASH the moment `org/h2/` is JIT-eligible
— confirmed, reproducible, genuine JIT regressions. `TestFileSystem`
hangs identically in BOTH arms, so its failure is **not** attributable
to this ban at all (see the separate note below) — it is excluded from
the decision to keep this ban.

## Bug 1 (new): `TestPageStoreCoverage` + `TestReopen` share ONE root cause

Both crash with the identical stack:

```
Caused by: java/lang/InternalError: JIT dispatch into
org/h2/mvstore/type/DataType.read(Ljava/nio/ByteBuffer;)Ljava/lang/Object;
failed: internal error: precise deoptimization unavailable for
org/h2/mvstore/db/RowDataType.read(Ljava/nio/ByteBuffer;)Ljava/lang/Object;
at bci 50; refusing side-effecting replay
```

This is `vm/src/runtime/interpreter.rs:7496-7503`'s deliberate safety
refusal: when a JIT-compiled method has already executed side effects
past bci 0 and hits a deopt trap, the VM requires a *precise* resume
(`compiled.can_deopt_resume` true, `deopt_frame_matches_method` true,
`build_deopt_frame_inner` succeeds) — reconstructing the interpreter
frame exactly rather than re-running the whole method from bci 0 (which
would re-execute already-committed side effects). For
`RowDataType.read` at bci 50, precise resume is unavailable, so the VM
correctly refuses rather than silently corrupting state — but that
refusal surfaces to H2 as an uncaught `InternalError`, which is this
crash.

This is NOT a new bug class — it's the same precise-deopt/frame-map
coverage gap the `precise-maps` effort
(`docs/known-issues/*precise-jit-maps*`, `real-frame-deopt-phaseA-landed`,
`A2 ReflRepro`) has been closing method-by-method. `RowDataType.read`
(and whatever `DataType.read` dispatches through it) is a method that
gap doesn't cover yet. Deliberately NOT attempting a fix here — this
subsystem has multiple concurrent sessions actively extending its
coverage; the fix belongs with that effort, not as a one-off patch.

**Repro** (isolated, no full suite needed):

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh discover   # if meta/ missing in your worktree
TMPDIR=/data/tmp H2_ROOT=<worktree>/apps/h2database/h2 \
  CRATONVM_BIN=<binary> CRATONVM_JIT_ALLOW_PACKAGES='org/h2/' \
  OUTROOT=<out> ./run-h2-suite.sh run --category all \
  --only 'TestPageStoreCoverage|TestReopen' --tag repro
```

**How to restore evidence for lifting this ban once fixed**: re-run the
table above; both classes should flip to PASS with the ban lifted once
`RowDataType.read`/`DataType.read`'s deopt-resume path is covered.

## Bug 2 (new, distinct root cause): `TestRunscript` data-correctness

```
Exception in thread "main" java/lang/AssertionError: expected: INSERT
INTO "PUBLIC"."TEST2" VALUES(462);
	at org/h2/test/db/TestRunscript.main(TestRunscript.java:40)
	at org/h2/test/TestBase.assertEqualDatabases(TestBase.java:1378)
```

`TestRunscript.test` dumps a database to a script, reloads it into a
second database, and diffs the two via `assertEqualDatabases` — the
diff disagrees once `org/h2/` is JIT-compiled. This is a genuine
data-divergence, not a crash/refusal like Bug 1, and does NOT share Bug
1's stack (no `RowDataType`/`DataType` frames in this log at all) — a
distinct defect. Not bisected further this session; the `try_patch_i32:
offset out of bounds; marking buffer overflowed` JIT warning appears
373 times in this class's log, but that same warning also appears in
classes that PASS (`TestFreeSpace` 13x, `TestStreamStore` 66x,
`TestNestedJoins` 327x), so it is very likely benign/unrelated rather
than this bug's cause — flagging that correlation check so the next
session doesn't waste time re-deriving it.

**Repro**: same command as above with `--only TestRunscript`.

## Not a regression of this ban: `TestFileSystem` hangs unconditionally

`TestFileSystem` (`memLZF:`/`nioMemLZF:` `testConcurrent`) now hangs to
the 300s runner timeout with **no exception, in both arms** — ban lifted
and ban active alike. The 2026-07-26 doc recorded this same class as an
intermittent CORRECTNESS failure (`Expected: 3900 actual: 3897`), not a
hang, so something changed since then independent of `org/h2/`'s JIT
eligibility. Candidate causes not yet investigated: the atomic-array fix
itself (a real exclusive lock where there used to be none could turn a
racy-but-live spin loop into genuine starvation/livelock under the
`while (!locks.compareAndSet(pos, 0, 1)) {}` idiom) or host flakiness
(this Azure host's `/` was at 96% full during this session, a condition
independently linked to prior flapping — see
`azure-host-disk-full-flapping-20260715.md`). Excluded from this ban's
decision because it reproduces identically with `org/h2/` fully
interpreted. Left for a future session; a thread dump
(`kill -QUIT` / `jstack`-equivalent) on a hung repro would be the fastest
next step.

## Conclusion

`HIB-LONGTAIL.1`'s `org/h2/` guard in `vm/src/jit/skip_list.rs` STAYS.
Two confirmed, reproducible, genuine correctness regressions block
lifting it (Bug 1: shared precise-deopt gap; Bug 2: distinct data
divergence), both freshly re-verified against current `dev`
(`072f3de9c`) with a clean A/B control. `TestFileSystem`'s hang is
real but orthogonal to this ban and tracked separately above.

See also: `docs/known-issues/h2/h2-jitban-residuals-20260726.md` (prior
history, atomic-array root cause), `vm/src/jit/skip_list.rs`'s
`HIB-LONGTAIL.1` comment, and
`docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`.
