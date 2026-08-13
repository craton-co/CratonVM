# `Files.newInputStream` read the whole file up front — `/dev/urandom` grew RSS to 14 GB until the kernel killed the process — FIXED 2026-07-31

## Status
**FIXED** — `fix/filechannel-map-retention-20260731`, merged to `dev`.
Retired here from `docs/known-issues/h2/bug-filechannel-map-anon-memory-growth.md`.

**The original report's root cause was wrong.** It blamed `FileChannel.map`'s
snapshot copy and the `MMAP_REGISTRY` lifetime. Neither is the defect; both
were measured innocent (see "What the original report got wrong"). The actual
cause was `Files.newInputStream`.

## What was wrong
`FileSystemProvider.newInputStream` and `Files.newInputStream` (three separate
registrations, all in `native-builtins/src/phases_late/nio_file.rs`) read the
**entire** path into memory with `std::fs::read` and handed back a
`ByteArrayInputStream` over the snapshot.

`std::fs::read` on a character device never returns. Lucene's
`org.apache.lucene.util.StringHelper.<clinit>` does exactly this, to get eight
bytes of seed:

```java
try (DataInputStream is = new DataInputStream(Files.newInputStream(Paths.get("/dev/urandom")))) {
    x0 = is.readLong();
}
```

So the first time anything touched Lucene, CratonVM started reading
`/dev/urandom` into an unbounded `Vec<u8>` at ~600 MB/s until the kernel's OOM
killer took the process.

## Why it looked like a heap/GC problem and wasn't
- **`-Xmx` is irrelevant.** The `Vec<u8>` is native memory. Peak RSS on the H2
  `FULLTEXT_LUCENE` repro, measured across four heap sizes:

  | `--Xmx` | peak RSS |
  |---------|----------|
  | 128m | 14 940 MB |
  | 512m | 13 474 MB |
  | 1g   | 13 806 MB |
  | 4g   | 16 276 MB |

- **No Java exception, no VM diagnostic.** Death is a kernel SIGKILL —
  `exit=137`, empty stdout. The only evidence is
  `sudo dmesg -T | grep 'Out of memory'`. Easy to misread as a hang.
- **The GC is not involved.** `CRATONVM_DBG=youngstate` shows zero
  `arena-grow` events; `--verbose:gc` shows a handful of ordinary minor GCs.
  The growth is entirely in mimalloc arenas.

## How it was found
1. `/proc/<pid>/smaps` during the repro: growth is `[anon:mimalloc]` regions,
   ~1 GB each, only ~124 VMAs total — bulk anonymous allocation, not thousands
   of leaked mappings.
2. `perf record -g --call-graph dwarf -e page-faults`: **100%** of page faults
   under `std::fs::read::inner` → `default_read_to_end` →
   `RawVec::grow_amortized` → `realloc` → `memcpy`. (The outermost frames the
   unwinder printed were garbage — `HprofWriter::write_full_heap_dump` off a
   `0x3` return address — but the inner chain was consistent and correct.)
3. `strace -e trace=openat`: only ~40 opens for the whole run, so it is not
   repeated file reading — one file being read forever. `/dev/urandom` is in
   the list.
4. `--stack-dump-on-timeout 25`: the last dispatch-trace entry is
   `org/apache/lucene/util/StringHelper.<clinit>` → `Paths.get` →
   `Files.newInputStream`.
5. Stage-by-stage RSS probes narrowed it to `new IndexWriter(...)`, then the
   `javap` of `StringHelper.<clinit>` named `/dev/urandom` outright.

## Fix
`fsp_new_input_stream` in `native-builtins/src/phases_late/nio_file.rs` — a
**lazy** stream, mirroring the existing `fsp_new_output_stream`:

- Open the path through `fd_table` and return a real
  `java.io.FileInputStream` whose `FileDescriptor` carries the fd. Every
  `read`/`skip`/`available`/`close` native already recovers it from there.
- Jar/VFS entries keep the in-memory `ByteArrayInputStream`: they have no file
  descriptor, and they are bounded by the entry size.
- Typed NIO errors preserved — `NoSuchFileException` / `AccessDeniedException`,
  not a bare `IOException`.
- All three `newInputStream` registrations now route to this one helper.
  (They are three registrations of the *same* `(class, name, descriptor)`;
  registration is last-writer-wins, so two of them were dead code. Two also
  copied the bytes one `set_array_element` call at a time.)

One trap worth recording: `FileInputStream.<init>` is itself natively
intercepted (`native_fis_open0`), so **the instance initialiser never runs on
any path** and `closeLock` stays null — the JDK's `close()` opens with
`synchronized (closeLock)`, so every try-with-resources NPEs. Constructing via
`new_object_initialized(...)` does not help for the same reason. native-io
already has `fis_backfill_constructor_fields` for exactly this; the fix fills
`fd` / `path` / `closeLock` / `closed` the same way.

## Verification
Host: Azure Linux box, `--java-home /home/victor/jdk25` (Temurin 25.0.3), `--nojit`.

- **`StreamProbe`** (checked in at
  `docs/known-issues/repros/newinputstream/StreamProbe.java`) — 12 checks
  covering the `/dev/urandom` and `/dev/zero` cases, both the `Files` and
  `FileSystemProvider` entry points, whole-file round-trip, single-byte reads,
  `skip`, read-past-EOF, `available`, `BufferedReader` lines, the
  `NoSuchFileException` contract, and a jar entry through
  `FileSystems.newFileSystem`. **Every line matches real HotSpot exactly.**
  On `dev` the probe produced *no output at all* — OOM-killed on check 1.
- **Lucene**: `new IndexWriter(...)` now completes with flat RSS (273 MB);
  before, it never returned.
- **H2** `TestFullText` / `TestRecovery`: `exit=137` (SIGKILL after ~14 GB) →
  `exit=1` (an ordinary, catchable Java exception), RSS flat throughout.
- **No regression**: `MapDiag map` 200 × 8 MiB peaks at 276 MB before vs
  277 MB after; a 7-jar / 12 642-entry `getName`/`getRealName`/`getSize` scan
  hashes identically before and after.
- `cargo test -p cratonvm-native-builtins`: 3144 passed, 0 failed.

## What the original report got wrong
Worth reading if you are tempted to trust its numbers:

- **`FileChannel.map` does not leak.** `MapDiag map` (200 iterations, 8 MiB
  each) peaks at 276 MB and does not grow with iteration count.
- **The original `MapLeak` numbers were noise.** Re-measured on a quiet host
  they are *non-monotonic*: n=100 → 638 MB, n=200 → 665 MB, n=400 → 295 MB.
  The claimed "≈4.1 MB retained per 8 MiB map, linear in iteration count" was
  peak-RSS variation from GC timing on a host at load average 30, not a leak.
  Two points on a noisy curve are not a trend — the third point refutes it.
- **The retained kernel mappings are not a distinctive defect.** After 200
  unclosed mappings CratonVM's `VmSize` is 2 998 MB against real HotSpot's
  4 998 MB. HotSpot's own `Cleaner` is equally lazy; CratonVM retains *less*.
- The snapshot copy in `native_fc_map` is a genuine cost (RSS 655 MB vs
  HotSpot's 79 MB over 200 maps) and skipping it for `READ_ONLY` mappings is
  still worth doing — but it is bounded by GC, it is not what killed anything,
  and it is not tracked as a bug.

## The blocker behind this one — also fixed (2026-07-31)
With this landed, the two H2 classes hit `EOFException: Unexpected EOF` out of
CratonVM's own `ByteBuffersDataInput` natives during Lucene's LZ4
preset-dictionary compression — proven independent of this fix at the time,
since with `-Dtests.seed=deadbeef` (which makes Lucene skip the `/dev/urandom`
read) the unmodified `dev` binary reached the identical exception at the
identical point with identical RSS.

That defect, and a second one behind it (`Lookup.findVirtual` never throwing
`NoSuchMethodException`, so H2's Lucene-version probe could not take its
fallback), are fixed in
`docs/internal/bug-lucene-bbdatainput-size-field-FIXED-20260731.md`.
**`org.h2.test.db.TestFullText` and `org.h2.test.unit.TestRecovery` now pass.**
