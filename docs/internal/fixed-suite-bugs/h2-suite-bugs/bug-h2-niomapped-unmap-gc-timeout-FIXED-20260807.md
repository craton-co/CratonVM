# `nioMapped:` — `Timeout (10000 ms) reached while trying to GC mapped buffer` — FIXED 2026-08-07

**Status: ✅ FIXED.** Found 2026-08-02, root-caused and fixed 2026-08-07. The
retention was not in anything about mapped buffers, weak references or H2 — it
was the conservative JIT root scan marking the whole native stack because a
*returned* compiled frame had left a return address behind.

## The failure

```
java.io.IOException: Timeout (10000 ms) reached while trying to GC mapped buffer
        at org.h2.store.fs.niomapped.FileNioMapped.unMap(FileNioMapped.java:68)
        at org.h2.store.fs.niomapped.FileNioMapped.implCloseChannel(FileNioMapped.java:108)
        at org.h2.test.unit.TestFileSystem.testPositionedReadWrite(TestFileSystem.java:572)
```

`FileNioMapped.unMap()` is H2's workaround for
[JDK-4724038](https://bugs.openjdk.org/browse/JDK-4724038): there is no public
unmap API, so it makes the collector do it and spins for 10 s.

```java
WeakReference<MappedByteBuffer> bufferWeakRef = new WeakReference<>(mapped);
mapped = null;
while (bufferWeakRef.get() != null) {
    if (System.nanoTime() - stopAt > 0L) { throw new IOException("Timeout ..."); }
    System.gc();
    Thread.yield();
}
```

So the claim the VM failed is: *a `MappedByteBuffer` whose only remaining
reference is a `WeakReference` must become collectable within 10 s of repeated
`System.gc()`.*

## The repro is 12 seconds, not 13 minutes

The original page said the retention needed "a long-running process, a large
populated heap, thousands of prior mappings, and JIT-compiled H2 frames", and
put the repro at ~784 s into a `TestFileSystem` run. Only the last of those is
true, and one prior filesystem is enough to arrange it:

| sequence | result |
|---|---|
| `nioMapped:` alone | **OK, 1.1 s** |
| plain disk, then `nioMapped:` | **FAIL, 10.6 s** — 3/3 |
| plain disk, then `nioMapped:`, `--nojit` | **OK, 1.0 s** — 3/3 |

`apps/h2database-suite-runner/probes/TfsProbe.java` drives
`TestFileSystem.testFileSystem(String)` one prefix per argument, which is what
makes that table cheap to produce. Running the class itself is a poor instrument
for any single filesystem: two of its earlier prefixes take ~1000 s and ~260 s
(see *What this does not fix*).

## Root cause

`CRATONVM_DBG_ROOT_SOURCE` (a temporary instrument, not landed) reported the
`collect_roots` section that pushed the retained buffer, the stack slots behind
it, and the band those slots were in:

```
[rootsrc] GC#44 java/nio/DirectByteBuffer @0x2004244f940 rooted by
  14-jit-frame-conservative-scan
  slots=[0x777584dadbc0 0x777584dadbe0 … 0x777584db1d18]   (15 distinct words)
  band=[0x777584dad990,0x777584dc0000)  chain=0
  hit=0x74e8b15af2f8->0x74e8b1b12af5 sun/nio/ch/FileChannelImpl.implWrite
```

Read that bottom-up:

* **`chain=0`** — the JIT entry chain was empty. No compiled frame was live.
* **`hit=… FileChannelImpl.implWrite`** — `scan_active_jit_frames`'s
  unregistered-JIT-frame probe still found a plausible return address into JIT
  code. `implWrite` had long since returned; the word was its **residue**.
* **`band=[…990, …0000)`** — on that hit the probe marks
  `[scanner_sp, stack_high)`. With an empty chain `scanner_sp` is the
  *collector's own* stack pointer, so the band is the entire ~75 KB live native
  stack: the collector's frames, every interpreter and native Rust frame, and
  the leftovers of everything the thread had run.
* **`slots=[…]`** — 15 dead words in that band still held the
  `MappedByteBuffer`'s address, so it was marked on every collection and
  `bufferWeakRef.get()` never returned null.

The loop is self-sustaining: `bufferWeakRef.get()` is itself a reference-returning
call, so each iteration rewrites the buffer's address into the same stack region
the next collection is about to scan.

The probe exists for a real case — a compiled frame that is live without having
pushed a `JitEntryGuard`, canonically the process entry point (`Vm::invoke` →
compiled `main`). Nothing distinguished that from residue.

## The fix

`vm/src/jit/conservative_roots.rs` now tracks a per-thread residue high-water
mark: `JIT_RESIDUE_HI`, the highest `entry_sp` among JIT entries that have
returned (set in `pop_jit_entry` and in `prune_returned_jit_entries`). A
compiled frame writes only below its own `entry_sp`, so once it returns, every
return address it left into JIT code lies below that mark.

With an **empty** chain, the unregistered-frame probe now rejects a hit below the
mark as residue. The mark is monotonic: a frame entered at `sp` writes only below
`sp` and so cannot overwrite residue at or above `sp` — resetting it on each push
discarded exactly the higher-addressed leftovers of an earlier, shallower frame,
and `split:nioMapped:` still failed until that was corrected. The live guardless
frame the probe exists for sits above every JIT entry the run ever makes (all of
them are entered deeper than `main`'s own call site), so a monotonic bound keeps
it.

With a **non-empty** chain nothing changes: `search_lo` is already `cover_hi`, so
the probe only ever looks above the chain, and the region below it is scanned by
the chain walk itself.

`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` restores the old accept-everything
behaviour, which is how the A/B below was measured in one binary.

## Verification

Same binary, same host, `TfsProbe`, plain disk then the mapped filesystem:

| arm | `nioMapped:` | `split:nioMapped:` |
|---|---|---|
| fix ON (default) | **OK** 3/3 | **OK** 3/3 |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` | FAIL 2/2 | FAIL 2/2 |

Full prefix sequence in `TestFileSystem.test()` order — plain disk, `async:`,
`memFS:`, `memLZF:`, `nioMemFS:`, `rec:memFS:`, `cache:`, `nioMapped:`,
`encrypt:0007:`, `cache:encrypt:0007:`, `split:`, `split:nioMapped:` — is
`DONE failed=0`. Before the fix the same run was `failed=1` at `nioMapped:` and
again at `split:nioMapped:`.

60 H2 suite classes, both arms run concurrently on the same host from the same
binary, 150 s cap: **one class differs, and it is an improvement** --
`TestCluster` PASSes with the filter and FAILs without it. The other 59 are
identical, including the three FAILs (`TestFunctions`, `TestLargeBlob`,
`TestOutOfMemory`) and seven HANGs that both arms share.

`cargo test -p cratonvm-gc -p cratonvm-vm -p cratonvm-jit -p cratonvm-native-io`
matches the pristine `dev` baseline measured the same way on the same host:
`cratonvm-vm --lib` 2449/2449 in both, and the same three pre-existing
`jit_local_exception_handler_tests` failures in both.

## What this does not fix

`TestFileSystem` as a whole still cannot finish inside a sane per-class cap, for
reasons that have nothing to do with this page:

| prefix | HotSpot | CratonVM |
|---|---|---|
| `nioMemLZF:1:` | 0.8 s | **995 s** |
| `encrypt:0007:` | 1.3 s | **261 s** |
| everything else | ≤1.3 s | 1–7 s |

`nioMemLZF:` is the residual the retired `h2-jitban-longtail1` write-up hands off
(per-element `DirectByteBuffer` accessor dispatch — the block comment above
`DbbElemFields` in `native-io/src/direct_buffer.rs` carries it). `encrypt:` is
the same shape one layer up. Until those close, `TfsProbe` is the acceptance
instrument for this class, not the class itself.

Separately, a leak found while measuring this: `temporary_direct_buffer_release`
in `native-io/src/direct_buffer.rs` takes a global root per released NIO
temporary direct buffer and `temporary_direct_buffer_get` only releases it on the
failure path, so every reuse leaks one JNI global reference — ~25,000
`DirectByteBuffer`s rooted from `roots.rs` section 9 in a 15 s run. Not on this
page's path; filed separately.

## Reproducing

```bash
H2=<h2 checkout>/h2
javac -cp $H2/target/classes:$H2/target/test-classes -d /tmp/tfs \
  apps/h2database-suite-runner/probes/TfsProbe.java
<binary> --java-home <jdk25> --Xmx 1g \
  -c $H2/target/classes:$H2/target/test-classes:/tmp/tfs \
  TfsProbe @BASE@/fs nioMapped:@BASE@/fs
```

`@BASE@` expands to `TestBase.getBaseDir()`. Run it from a scratch directory —
H2's `BASE_TEST_DIR` is `./data`, relative to the working directory.
