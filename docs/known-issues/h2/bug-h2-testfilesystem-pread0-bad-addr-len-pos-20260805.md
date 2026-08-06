# `TestFileSystem` dies in ~1 s: `IOException: pread0/pwrite0: bad addr/len/pos`

**Status: OPEN, REGRESSION.** Found 2026-08-05. Not H2-specific and not about
any filesystem prefix H2 layers on top — this is the *plain disk* one.

## Symptom

```
java.lang.AssertionError: Exception: java.io.IOException: pread0: bad addr/len/pos
        at org.h2.test.unit.TestFileSystem.testConcurrent(TestFileSystem.java:791)
        at org.h2.test.unit.TestFileSystem.testFileSystem(TestFileSystem.java:374)
        at org.h2.test.unit.TestFileSystem.test(TestFileSystem.java:60)
```

`test:60` is `testFileSystem(getBaseDir() + "/fs")` — the **first** filesystem
the class exercises, an ordinary on-disk one, before any of the `memFS:` /
`memLZF:` / `nioMem*:` / `nioMapped:` prefixes. The whole class is therefore
unreachable: it dies in **0.5-1.3 seconds**.

Both `pread0` and `pwrite0` produce it; which one surfaces varies with timing,
because `testConcurrent` runs a reader thread against a writer.

## It is a regression, and a recent one

Same runner, same H2 checkout, same 1200 s cap, one class at a time:

| binary | result |
|---|---|
| `dev` @ `86a01abf90` (2026-08-02) | HANG at the cap — slow, but no `pread0` |
| `dev` @ 2026-08-05 | **FAIL 1.3 s**, `pwrite0: bad addr/len/pos` |
| 2026-08-05 + the DirectByteBuffer-reclamation work | **FAIL 0.5 s**, `pread0: bad addr/len/pos` |

The last two rows are the same failure; the third is listed only to record that
the reclamation work does not cause or cure it. So the defect arrived somewhere
in the 2026-08-02 → 2026-08-05 span, which contains a large amount of NIO and
native-dispatch churn.

## Why it matters beyond this class

`pread0` / `pwrite0` are the positional-I/O natives behind
`FileChannel.read(ByteBuffer, long)` / `write(ByteBuffer, long)` — the API every
database-shaped workload uses. A refusal on "bad addr/len/pos" means the native
is rejecting arguments the JDK considers valid, so the blast radius is any
positional channel I/O, not just H2's test harness.

## Reproducing (seconds)

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh discover     # required in a fresh worktree; meta/ is not committed
TMPDIR=/data/tmp H2_ROOT=<h2 checkout> CRATONVM_BIN=<binary> \
  OUTROOT=<out> CLASS_TO=120 \
  ./run-h2-suite.sh run --category all --only TestFileSystem --tag pread0
```

The failure is in the first second or two of the run, which makes this a very
cheap bisect target across the 2026-08-02 → 2026-08-05 range.

## Next step

Print the rejected `(addr, len, pos)` triple at the refusal site before
bisecting — the three inputs distinguish "the buffer address is wrong" (which
would point at the direct-buffer/`Unsafe`-arena boundary) from "the length or
position is wrong" (which would point at the channel layer). The message names
all three but does not currently include their values.

## Context

This is what `TestFileSystem` fails with *today*. The retired
`h2-jitban-longtail1` write-up records that on 2026-08-02 the class got as far
as `nioMapped:` once the `nioMemLZF:` throughput gap was closed, and hands off a
separate `nioMapped:` blocker in
`bug-h2-niomapped-unmap-gc-timeout.md`. Both of those observations are still
valid; they are simply unreachable until this one is fixed.
