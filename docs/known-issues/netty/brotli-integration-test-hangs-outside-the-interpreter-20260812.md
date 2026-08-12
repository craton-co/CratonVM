# `BrotliIntegrationTest` hangs before any test runs, somewhere the stack-dump watchdog cannot reach

**Status:** OPEN (2026-08-12). The one HANG on
[netty investigate-batch-04](investigate-batch-04.md). Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `8763197f2`.

## Symptom

`io.netty.handler.codec.compression.BrotliIntegrationTest` runs 11/11 in
**9.4 s** on stock HotSpot JDK 25. On CratonVM it never finishes — killed at
the 600 s and 900 s harness caps in two separate runs, with **no `@@RESULT`
line**, so not a single test was reported.

It is a genuine hang, not the `io.netty.buffer` throughput gap: the VM stops
producing output entirely. In a 200 s run the last log line is at **t+3 s**
(the routine post-clinit `Unsafe MEMORY_ACCESS_OPTION` fixup and a GC-guard
warning), and nothing is emitted for the remaining ~197 s.

## The interesting part: the watchdog never fires

Re-run with `--stack-dump-on-timeout 60`, which arms a watchdog that dumps
every thread's Java frame chain and aborts:

```
[cratonvm] stack-dump watchdog armed: will dump + abort after 60s
... last log line at t+3s ...
(no "stack dump" record ever appears; process still alive at t+200s)
```

The watchdog is driven from the interpreter's dispatch loop, so a thread that
is not executing bytecode cannot be sampled or interrupted by it. That the
60 s dump never happened is evidence the VM is blocked **outside the
interpreter** — in a native call, a library load, or a lock held across one —
rather than deadlocked in Java code. A Java-level deadlock would have produced
a dump naming the frames.

## Leading hypothesis

`BrotliIntegrationTest` needs `com.aayushatharva.brotli4j`, which loads a
bundled platform-native `.so` through `System.loadLibrary`/JNI. The suite's
other JNI-native codec classes (Zstd, LZ4) were the subject of
`docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md`,
so this is the same neighbourhood — but that one crashed and this one blocks,
so do not assume the same defect. It has not been root-caused; the point of
this page is the *shape* (hangs pre-test, invisible to the interpreter
watchdog), which tells the next person which instrument to bring.

## Suggested next step

The interpreter watchdog is the wrong instrument here. Attach to the live
process from outside — `gdb -p <pid>` / `eu-stack -p <pid>` for the **native**
stack, or `perf record -p <pid>` for a few seconds to see whether it is
spinning or blocked. That distinguishes a `loadLibrary` deadlock from a native
spin from a blocking read, which the Java-side evidence cannot.

Worth checking first, because it is nearly free: whether the class gets as far
as `Brotli.ensureAvailability()` at all, by running a bare
`System.loadLibrary`-only probe against brotli4j with no netty and no JUnit.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.compression.BrotliIntegrationTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 600 \
  --bin <cratonvm> --out /tmp/repro          # no @@RESULT, killed at the cap

# HotSpot, same classpath: 11/11 in 9.4s
CP=$(sed -n 2p common.args)
/data/toolchain/jdk-25/bin/java -cp "$CP" -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.BrotliIntegrationTest
```
