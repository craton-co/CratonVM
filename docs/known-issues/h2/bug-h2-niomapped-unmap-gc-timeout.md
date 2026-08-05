# `nioMapped:` — `Timeout (10000 ms) reached while trying to GC mapped buffer`

**Status: OPEN**, found 2026-08-02. Blocks `org.h2.test.unit.TestFileSystem`.
Pre-existing, but only newly *reachable*: until the `nioMemLZF:` per-element
throughput gap was closed (see the retired `h2-jitban-residuals-20260726`
write-up), the class never survived long enough to get here.

## The failure

```
java.io.IOException: Timeout (10000 ms) reached while trying to GC mapped buffer
        at org.h2.store.fs.niomapped.FileNioMapped.unMap(FileNioMapped.java:68)
        at org.h2.store.fs.niomapped.FileNioMapped.implCloseChannel(FileNioMapped.java:108)
        at org.h2.test.unit.TestFileSystem.testPositionedReadWrite(TestFileSystem.java:572)
        at org.h2.test.unit.TestFileSystem.testFileSystem(TestFileSystem.java:376)
        at org.h2.test.unit.TestFileSystem.test(TestFileSystem.java:95)
```

`TestFileSystem.test:95` is `testFileSystem("nioMapped:" + getBaseDir() + "/fs")`
— i.e. the class has already cleared `memFS:`, `memLZF:`, `nioMemFS:`,
`nioMemLZF:1:`, `nioMemLZF:12:`, `rec:memFS:`, `testUserHome()` and `cache:`.
It reaches this at ~784 s of a 900 s cap.

`FileNioMapped.unMap()` is H2's workaround for
[JDK-4724038](https://bugs.openjdk.org/browse/JDK-4724038) — a mapped region
cannot be truncated or deleted on some platforms while it is still mapped, and
there is no public unmap API, so H2 makes the collector do it:

```java
WeakReference<MappedByteBuffer> bufferWeakRef = new WeakReference<>(mapped);
mapped = null;
long stopAt = System.nanoTime() + GC_TIMEOUT_MS * 1_000_000L;
while (bufferWeakRef.get() != null) {
    if (System.nanoTime() - stopAt > 0L) {
        throw new IOException("Timeout (" + GC_TIMEOUT_MS + " ms) reached ...");
    }
    System.gc();
    Thread.yield();
}
```

So the claim the VM fails is: *a `MappedByteBuffer` whose only remaining
reference is a `WeakReference` must become collectable within 10 s of repeated
`System.gc()`.*

## What is established

**It is not a general weak-reference failure, and not specific to mapped
buffers.** `probes/WeakGcProbe.java` weakly references three shapes and spins
the same loop:

| referent | HotSpot | CratonVM |
|---|---|---|
| plain `Object` | collected, 1 `System.gc()` | collected, 1 `System.gc()` |
| direct `ByteBuffer` | collected, 1 `System.gc()` | collected, 1 `System.gc()` |
| `MappedByteBuffer` | collected, 1 `System.gc()` | collected, 1 `System.gc()` |

**H2's own `unMap()` path is clean in isolation, on this build AND on the
unmodified `origin/dev` one.** `probes/NioMappedProbe.java` drives
`FileNioMapped` through eight growth `reMap()`s (each of which `unMap()`s the
previous mapping), reads every block back, and closes the channel:

| | round 0 | round 1 | round 2 |
|---|---|---|---|
| HotSpot | 200.0 ms | 68.1 ms | 60.5 ms |
| CratonVM `origin/dev` | 288.4 ms | 220.4 ms | 223.3 ms |
| CratonVM (this work) | 256.7 ms | 200.4 ms | 207.6 ms |

Both `OK`. So the retention needs the *context* `TestFileSystem` builds up and
the probe does not — a long-running process, a large populated heap, thousands
of prior mappings, and JIT-compiled H2 frames on the stack.

## Leading hypothesis, not yet confirmed

Conservative stack roots. `unMap()` is reached from
`implCloseChannel` → `testPositionedReadWrite`, several frames deep, after a
loop that has been running long enough to be JIT-compiled. A stale spill slot or
operand-stack slot in one of those compiled frames still holding the old
`MappedByteBuffer` would root it for as long as the frame lives — which is
exactly "not reproducible in a short probe, permanent inside the real test", and
is a known family (`native-stale-local-family-and-persistent-singleton-roots`,
`rootsnap-cache-stale-reassigned-local`).

**The cheap next step** is to run `TestFileSystem` with `--nojit`: if it clears
`nioMapped:` interpreted and hangs here compiled, the hypothesis is confirmed
and the work moves to the conservative-root scan rather than to anything about
mapped buffers. If it fails both ways, the retention is elsewhere and the next
instrument is a reachability dump of the buffer at the moment the loop gives up.

## Reproducing

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh discover        # required in a fresh worktree; meta/ is not committed
TMPDIR=/data/tmp H2_ROOT=<h2 checkout> CRATONVM_BIN=<binary> \
  OUTROOT=<out> CLASS_TO=900 \
  ./run-h2-suite.sh run --category all --only 'TestFileSystem' --tag niomapped
```

~13 minutes to the failure. The isolated probes above are seconds:

```bash
H2CP=<h2>/target/classes
javac -cp $H2CP -d /tmp/probe apps/h2database-suite-runner/probes/NioMappedProbe.java
javac -d /tmp/probe apps/h2database-suite-runner/probes/WeakGcProbe.java
<binary> --java-home <jdk25> -c $H2CP:/tmp/probe NioMappedProbe /tmp/nmp 3
<binary> --java-home <jdk25> -c /tmp/probe WeakGcProbe 5000
```

## Note for whoever picks this up

H2 has an escape hatch this VM could satisfy instead:
`SysProperties.NIO_CLEANER_HACK` (`-Dh2.nioCleanerHack=true`) makes `unMap()`
call `org.h2.util.MemoryUnmapper.unmap(mapped)` and return immediately, with no
GC loop at all. That is a *workaround for the test*, not a fix for the VM — the
retention it papers over is real and would surface again in any application that
uses the same JDK-4724038 idiom.
