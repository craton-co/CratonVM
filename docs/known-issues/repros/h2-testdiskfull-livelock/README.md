# `TestDiskFull` livelock reproducer

Harness used to root-cause the `org.h2.test.synth.TestDiskFull` hang
(2026-08-01), and to re-verify the verdict on 2026-08-02.

**The verdict, so nobody re-derives it:** `TestDiskFull` wedging in the H2 suite
is an **upstream H2 defect**, not a CratonVM one.
`TransactionStore.endLeftoverTransactions()` calls `commit()` on a recovered
`STATUS_COMMITTED` transaction, whose `commit()` then takes neither
`store.commit(…)` nor `close()` because both are guarded by `wasActive`, which
is false for exactly those transactions. The recovered transaction keeps its map
entries locked for the life of the store, and a later write to one of those keys
spins forever in `TransactionMap.set` — `Transaction.waitForThisToEnd` returns
`true` immediately for a blocker parked in `STATUS_COMMITTED`, so `LOCK_TIMEOUT`
never fires. It is reachable on stock HotSpot (28 unapplied-commit events in 150
runs there); CratonVM walks into it far more often only because it issues ~3.5×
more file write operations for the same logical work, which lands the injected
failure at an earlier logical point where the leftover holds a `table.0` meta key
that database reopen rewrites. Do **not** patch `apps/h2database` — the suite
runs stock code. Full analysis, measurements and controls: the retired
`h2-testdiskfull-upstream-transaction-recovery-livelock` write-up.

Re-verified 2026-08-02 on `origin/dev@86a01abf90`: 8 of 10 runs livelock, same
leftover keys (`table.0` 2/3/4) as on 2026-08-01.

Nothing in the shared `apps/h2database` checkout is touched: a patched copy of
three classes is compiled into a private directory that is **prepended** to the
classpath.

## What the overlay changes

| class | change |
| --- | --- |
| `org.h2.test.synth.TestDiskFull` | `DFULL_MAX` / `DFULL_START` / `DFULL_WRITE_DELAY` env knobs, per-iteration timing, raw write-op count. `test(int)` is byte-identical to upstream. |
| `org.h2.mvstore.tx.Transaction` | `[cvm-spin]` report every 20 000 `waitFor` calls (blocker id, blocker status, slot occupancy, committing bit); `[cvm-commit-exit]` / `[cvm-commit-throw]` around `commit()`. |
| `org.h2.mvstore.tx.TransactionStore` | `[cvm-leftover]` + `[cvm-leftover-rec]` for every transaction recovered by `init()`, including its undo-log records (map name + key); `[cvm-endleftover]`. |

Env knobs:

* `DFULL_MAX` — absolute iteration count for the `test(i)` loop (upstream picks
  `min(1000, writeOps + 10)`, which is unbounded in practice and, when the warmup
  throws, overflows negative and skips the loop entirely).
* `DFULL_START` — first `i`. Raising it moves the injected disk-full failure to a
  later logical point; `DFULL_START=250` removes the livelock.
* `DFULL_WRITE_DELAY` — the URL's `WRITE_DELAY` (default `10`, upstream's value).
  `3000` effectively disables the MVStore background writer and removes the
  livelock.

## Use

```bash
export H2=/path/to/apps/h2database/h2       # must be built (target/classes, target/test-classes,
                                            # craton-testcp.txt)
export JAVA_HOME_25=/path/to/jdk25
export OV=/tmp/dfull-ov                     # overlay output dir

./build-overlay.sh

# CratonVM arm: 10 runs, 4 at a time, 300 s timeout, 60 iterations each
DFULL_MAX=60 ./run-arm.sh /path/to/cratonvm-binary cvm 10 4 300

# stock HotSpot control
DFULL_MAX=60 ./run-arm.sh HOTSPOT hs 10 4 300

# the two controls that remove the hang
DFULL_MAX=60 DFULL_WRITE_DELAY=3000 ./run-arm.sh /path/to/cratonvm-binary wd3000 10 4 300
DFULL_MAX=60 DFULL_START=250        ./run-arm.sh /path/to/cratonvm-binary late  12 4 300
```

`run-arm.sh` writes `$OUT/results.txt` with one line per run and prints a class
histogram. Useful greps afterwards:

```bash
grep -c 'cvm-spin'                  $OUT/run-*.log   # wedged runs
grep -c 'wasActive=false'           $OUT/run-*.log   # unapplied COMMITTED leftovers
grep    'cvm-leftover-rec'          $OUT/run-*.log   # which key each leftover holds
grep    'raw write ops in warmup'   $OUT/run-*.log   # write-op density
grep -c 'out-of-bounds field'       $OUT/run-*.log   # the retired heap-corruption signature
```

Add `CRATONVM_DEFAULT_WATCHDOG_SEC=240` to turn a wedge into a full stack dump
(`gdb` cannot attach on the Azure host, and `jcmd Thread.print` returns frameless
threads).

## Extra note

`EXTRA=--nojit` reproduces the livelock as well — it is not a JIT bug.

The CratonVM heap-corruption symptoms this class used to show
(`class_id=ClassId(0)` guard burst, `ClassCastException`) did not occur once in
265 runs of this harness, nor in 330 runs of the **stock** class, on
`dev@c8a3ba181d` — while a `dev` build from immediately before the post-GC
reference-processing fix reproduces them in 2 of 42 stock runs. Use the stock
class, not this overlay, for that A/B: the overlay bounds the iteration count and
therefore the exposure.
