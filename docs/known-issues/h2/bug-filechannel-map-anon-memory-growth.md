# `FileChannel.map` retains its whole mapping — anonymous RSS grows without bound and ignores `-Xmx`

## Status
**OPEN** — reproduced on unmodified `dev` (`a31a8a93f`) with a standalone,
14-line probe. Found 2026-07-31 while verifying the fix for
`Runtime.version()` (see
`docs/internal/bug-h2-runtime-version-tostring-npe-FIXED-20260731.md`): with
that NPE out of the way, H2's `FULLTEXT_LUCENE` finally reaches Lucene's
`MMapDirectory`, and that is what this bug kills.

## Severity
**HIGH** — the process is OOM-killed by the kernel (SIGKILL, exit 137), not by
a `java.lang.OutOfMemoryError`, so no application-level handling can help and
no VM diagnostics are printed. `-Xmx` does not bound it. Any workload that
memory-maps files in a loop is affected; Lucene (Elasticsearch, H2
`FULLTEXT_LUCENE`) is the obvious one, but so is any `MappedByteBuffer` user.

## Symptom
```
##### org.h2.test.db.TestFullText exit=137
```
`dmesg`:
```
Out of memory: Killed process 2257327 (cratonvm-rtv-fi)
  total-vm:55299808kB, anon-rss:19929356kB, ...
```
Run with `--Xmx 1g`. The memory is **anonymous and private-dirty**, not the
mapped files — `/proc/<pid>/smaps_rollup` during the H2 repro below:

| t | Rss | Anonymous | VMA count |
|---|-----|-----------|-----------|
| 10s | 2 689 480 kB | 2 655 128 kB | 124 |
| 16s | 5 180 100 kB | 5 163 184 kB | — |

124 VMAs total, so this is not "thousands of leaked mappings"; it is bulk
anonymous allocation. It also exceeds the `-Xmx 1g` ceiling by 5x, so it is
not (only) the ordinary Java heap.

## Repro (no Lucene, no H2, no `Runtime.Version`)
```java
// MapLeak.java
import java.io.*; import java.nio.*; import java.nio.channels.*; import java.nio.file.*;
public class MapLeak {
  public static void main(String[] a) throws Exception {
    int n = Integer.parseInt(a[0]);
    Path p = Path.of("maptarget.bin");
    Files.write(p, new byte[8 * 1024 * 1024]);
    long sum = 0;
    for (int i = 0; i < n; i++) {
      try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
        MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_ONLY, 0, ch.size());
        sum += m.get(0) + m.get((int) ch.size() - 1);
      }
    }
    System.out.println("sum=" + sum);
  }
}
```
```bash
/usr/bin/time -v <cratonvm> --java-home /home/victor/jdk25 --Xmx 1g --nojit -c . MapLeak 400
```

Peak RSS, same host, same 8 MiB file, channel closed and buffer dropped every
iteration:

| runtime | 100 iterations | 400 iterations |
|---------|----------------|----------------|
| HotSpot 25.0.3 | — | **118 MB** |
| CratonVM `dev` a31a8a93f | 662 MB | **1 895 MB** |

≈4.1 MB retained per 8 MiB mapping, linear in iteration count. HotSpot stays
flat because its mapping is file-backed and its `Cleaner` unmaps on collection.

The H2-level repro (needs the `Runtime.version()` fix to get this far):
```bash
cd /data/data/rtv-h2run/ftl   # FtlRepro.java: FTL_INIT / FTL_CREATE_INDEX / FTL_SEARCH
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g --nojit -c "$CP" FtlRepro
```
grows ~600 MB/s and is OOM-killed in under 30s. Real HotSpot completes it in
about a second.

## Where it comes from
`native-io/src/lib.rs`:

- `native_fc_map` snapshots the **entire** mapping into a Java `byte[]` —
  `let snapshot: Vec<u8> = entry.as_slice().to_vec();` (line ~14983), then
  `ctx.write_byte_array_from(arr, 0, &snapshot)`. So every `map()` costs a
  full second copy of the region on top of the kernel mapping. The doc comment
  above the function is candid that this is "a safe-but-complete model" chosen
  so the existing `ByteBuffer` opcodes keep working.
- The kernel mapping itself is held in `MMAP_REGISTRY`
  (`static MMAP_REGISTRY`, line ~14455) keyed by an id stored in the buffer's
  `MBB_FIELD_MAPPED_ADDR` slot, and is only dropped by an explicit `unmap0`.
  The registry's own invariant comment already documents the gap: *"If the GC
  collects an MBB whose entry is still live, the kernel mapping survives until
  process exit — a small resource leak but memory-safe."* Real JDK code never
  calls `unmap0` explicitly; it relies on the `MappedByteBuffer`'s `Cleaner`.

Two questions to settle before fixing:
1. Which of the two is the anonymous growth? The kernel mappings are
   file-backed and should show as file-rss, so the snapshot `byte[]`s (or
   whatever backs oversized arrays outside the `-Xmx` budget — cf.
   the humongous-object path) are the primary suspect.
2. Why does the growth ignore `-Xmx 1g`? A large-array allocation path that
   bypasses the heap budget would explain both the ceiling breach and the low
   VMA count.

## Suggested fix
Give the `MappedByteBuffer` a real release path tied to its lifetime rather
than to an explicit `unmap0` call: register a GC/`Cleaner`-driven hook that
drops the `MMAP_REGISTRY` entry (and its snapshot array) when the buffer
becomes unreachable. Separately, consider whether the snapshot copy can be
avoided for `READ_ONLY` mappings — that halves the cost even before the
lifetime issue is fixed.

## Not related to
The `Runtime.version()` lightweight-object bug this was found behind. Verified
by differential: the `MapLeak` numbers are identical (1 071 MB vs 1 073 MB at
200 iterations) with and without that fix, and `MapLeak` never touches
`Runtime.Version` at all.
