# `TestDiskFull` livelock reproducer

Harness used to root-cause the `org.h2.test.synth.TestDiskFull` hang
(2026-08-01). See
`docs/known-issues/h2/h2-testdiskfull-upstream-transaction-recovery-livelock.md`
for what it proved.

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
