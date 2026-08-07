# `nioMapped:` — the unmap GC timeout is back, and the compact-header fix did not fix it

**Status: OPEN**, 2026-08-07. Bisected. Reproduces on Linux and Windows, on
pristine `dev` and on any branch of it.

## Symptom

```
java.io.IOException: Timeout (10000 ms) reached while trying to GC mapped buffer
        at org.h2.store.fs.niomapped.FileNioMapped.unMap(FileNioMapped.java:68)
        at org.h2.store.fs.niomapped.FileNioMapped.setFileLength(FileNioMapped.java:164)
        at org.h2.store.fs.niomapped.FileNioMapped.write(FileNioMapped.java:196)
        at org.h2.test.unit.TestFileSystem.testConcurrent(TestFileSystem.java:722)
```

`FileNioMapped.unMap` drops its reference to a `MappedByteBuffer` and then spins
— `System.gc()`, `Thread.sleep`, retry — waiting for the buffer to become
unreachable and be collected, giving up after 10 s. It never is.

This is the failure the retired `bug-h2-niomapped-unmap-gc-timeout` write-up
closed (with the conservative-root / JIT-frame residue-band work). It is back.

## Bisected to the header change

One build per commit on the Azure Linux host, `LTO=thin`, driven by
`apps/h2database-suite-runner/probes/TfsProbe.java`:

| `dev` commit | `TfsProbe nioMapped:` |
|---|---|
| `9ddbc9c61` Merge feat/spring-boot-residual-rerun | **OK 1.0 s** |
| `6ba350cdd` Merge perf/header-16-and-field-packing-20260806: HEADER_SIZE 24 → 16 | timeout 10.0 s |
| `13d2e01b3` | timeout 10.0 s |
| `b7cbd0034` (tip at filing) | timeout 10.0 s |

## It is NOT the compact reference-field defect

The same merge produced a second, louder regression —
`FileChannelImpl.fileLockTable` reading null, written up in the retired
`compact-ref-field-layout-corrupts-filechannel-filelock` page and **fixed** in
`b7cbd0034`'s ancestry (one line in `try_thin_unlock`: the last thin-lock
release stored a bare `MARK_NEUTRAL` and erased the `kind`/`element_type`/
`gc_age`/`gc_flags` quartet, `GC_FLAG_COMPACT` among them).

Two measurements say this one is different:

1. **`CRATONVM_COMPACT_REF_FIELDS=0` does not help.** On `6ba350cdd` that flag
   turns the file-lock probe from `flt is null` to `PROBE-OK`, and leaves this
   timeout exactly where it was — 10.0 s either way.
2. **The `try_thin_unlock` fix does not help.** On `b7cbd0034`, plain-disk
   `TestFileSystem` passes again (0.7 s) and `nioMapped:` still times out.

So this is the *other* half of what `HEADER_SIZE 24 -> 16` changed: whatever
keeps a `MappedByteBuffer` reachable after its last Java reference is gone —
a conservative root, a stale mark base, or a cleaner/phantom-queue path that
reads the header — not the reference-field packing.

## Reproducing (10 s)

```bash
H2=<checkout>/apps/h2database/h2
javac -cp "$H2/target/classes:$H2/target/test-classes" -d /tmp/tfs \
  apps/h2database-suite-runner/probes/TfsProbe.java
cratonvm --java-home <jdk25> --Xmx 1g \
  -c "$H2/target/classes:$H2/target/test-classes:/tmp/tfs" \
  TfsProbe "nioMapped:@BASE@/fs"
```

Run it from a scratch directory — H2's `BASE_TEST_DIR` is `./data`, relative to
the working directory. HotSpot: `OK`. `dev` since `6ba350cdd`: `FAIL … 10.0 s`.

## Next step

Start from what the retired `bug-h2-niomapped-unmap-gc-timeout` write-up already
established about which root keeps the buffer alive, and re-ask it against the
16-byte header: the residue-band work there turned on reading a frame's return
address and on `is_plausible_header`-style checks, and both are exactly the kind
of predicate a header-layout change invalidates without changing any call site.
`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE` is the kill switch that write-up left
behind; whether it still moves this needle is the cheapest first measurement.

## Related

* the retired `bug-h2-niomapped-unmap-gc-timeout` write-up — what this re-opens.
* the retired `compact-ref-field-layout-corrupts-filechannel-filelock`
  write-up — the same merge's *other* regression, fixed, and the reason a fix
  for that one must not be read as a fix for this.
