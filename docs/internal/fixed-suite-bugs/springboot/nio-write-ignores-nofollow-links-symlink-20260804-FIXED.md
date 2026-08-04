# nio.file open paths silently ignored `LinkOption.NOFOLLOW_LINKS` (wrote through a symlink instead of throwing)

**Status: FIXED 2026-08-04** — branch `fix/nio-nofollow-links-write-20260804`.

## Symptom

`org.springframework.boot.system.ApplicationPidTests#whenSymlinkToTargetExistsAtPidFileLocationWriteThrows`
failed:

```
java.lang.AssertionError:
Expecting code to raise a throwable.
    org.springframework.boot.system.ApplicationPidTests.whenSymlinkToTargetExistsAtPidFileLocationWriteThrows(ApplicationPidTests.java:110)
```

The test creates a regular file `target`, a symbolic link `link -> target`,
then calls `ApplicationPid.write(link)` and asserts it throws an `IOException`.
On CratonVM no exception was thrown — the write silently succeeded, **through
the link, onto `target`**.

## Root cause

`ApplicationPid.write(File)` does

```java
Files.writeString(path, this.pid.toString(), StandardOpenOption.TRUNCATE_EXISTING,
        StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
```

`LinkOption.NOFOLLOW_LINKS` implements `OpenOption` as well as `CopyOption`, so
it is legal in every `open` / `new*Stream` / `write*` varargs list. Per spec,
when the **final** component of the path is itself a symbolic link, opening it
with `NOFOLLOW_LINKS` must fail: the platform providers add `O_NOFOLLOW` to the
open flags and the kernel refuses with `ELOOP` *before* anything is created or
truncated.

CratonVM's option scanner (`fsp_scan_open_options`,
`native-builtins/src/phases_late/nio_file.rs`) only looked for `APPEND` and
`CREATE_NEW`. `NOFOLLOW_LINKS` was never inspected by **any** open path, and the
underlying opens follow symlinks by default — so the option was a no-op
everywhere.

The original triage named `fsp_new_output_stream` as the site. That is a real
gap, but it is *not* the one the failing test reaches: `Files.writeString` is
registered as its own native and never went near it. That native was

```rust
match std::fs::write(&p, &content) {
    Ok(()) => ...,
    Err(e) => Err(RuntimeError::IllegalStateException { message: format!("IOException: {}", e) }.into()),
}
```

which carried three defects beyond the one under investigation:

1. it ignored **every** `OpenOption` — `APPEND` truncated, `CREATE_NEW`
   silently overwrote, `TRUNCATE_EXISTING`/`NOFOLLOW_LINKS` were dead letters;
2. it bypassed the capability gate entirely (GAP I2), leaving the whole
   `Files.write*` surface invisible to any installed path policy; and
3. it reported failure as `IllegalStateException`, which is **not** an
   `IOException` — so `catch (IOException)` in any caller of a method declared
   `throws IOException` could never match it, and neither could AssertJ's
   `assertThatIOException`. Even had the option been honoured, the test would
   still have failed on the exception type.

The same `std::fs::write` shape appeared in three sibling registrations
(`Files.write(Path, byte[], OpenOption...)` — registered twice — and the two
`Files.write(Path, Iterable, ...)` overloads).

## Fix

`fsp_scan_open_options` now returns a `P57OpenFlags { append, create_new,
nofollow }` and every open path consults `nofollow`:

| Path | Site |
| --- | --- |
| `Files.writeString`, `Files.write` (bytes ×2, iterable ×2) | new shared `p57_files_write_bytes` |
| `FileSystemProvider.newOutputStream` | `fsp_new_output_stream` |
| `Files`/`FileSystemProvider.newInputStream` | `fsp_new_input_stream` |
| `FileChannel.open`, `Files.newByteChannel` | `newFileChannel` (both funnel through it) |
| `Files.newBufferedWriter` | real path via `newOutputStream`; synthetic path via `open_buffered_writer` |

`p57_nofollow_reject` performs the check with an `lstat` (`symlink_metadata`,
which does not resolve the final component) immediately before the open, and
raises the plain `java.io.IOException` HotSpot raises. Verified against OpenJDK
21 on Linux: `UnixChannelFactory` special-cases `ELOOP` under `NOFOLLOW_LINKS`
and throws a bare `IOException` — *not* a `FileSystemException` — with the
message `Too many levels of symbolic links (NOFOLLOW_LINKS specified)` and no
path prefix.

The `Files.write*` statics were additionally routed through the gated open
(`capability_gate::open_write_gated`) the rest of the surface already used, so
they now honour `APPEND`/`CREATE_NEW`, are visible to the capability gate, and
throw `IOException` subclasses rather than `IllegalStateException`. Two
incidental correctness gaps in the code being rewritten were closed at the same
time: `Files.writeString` silently wrote an **empty file** for any
`CharSequence` that was not a `String` (`StringBuilder`, `CharBuffer` —
`read_string(...).unwrap_or_default()`), and `write_iterable_impl` held its
`Path` and `Iterator` across re-entrant `iterator()`/`hasNext()`/`next()`/
`toString()` calls without pinning them (native stale-`ObjectRef` family).

## Validation

Binaries: `cratonvm-nf-baseline` (dev `3db59eb9`) and `cratonvm-nf-fixed`,
release builds on the Azure Linux host. HotSpot reference as noted per row.

### 1. The reported test — `ApplicationPidTests`

SbRunner, real JDK 25 (`/data/jdk25-real-20260717/jdk-25.0.3+9`),
`core/spring-boot` test classpath.

| VM | Result |
| --- | --- |
| HotSpot 25 | `tests=13 failed=0` |
| CratonVM **baseline** | `tests=13 failed=1` — `whenSymlinkToTargetExistsAtPidFileLocationWriteThrows` |
| CratonVM **fixed** | `tests=13 failed=0` |

### 2. `probes/NoFollowLinksOpenProbe.java` — every open path

Nineteen assertions: seven refusals across `writeString` (live and dangling
link), `newOutputStream`, `newInputStream`, `newByteChannel`,
`FileChannel.open` and `newBufferedWriter`; "the target was not touched" after
each; and three families of negative control (dropping `NOFOLLOW_LINKS` must
write *through* the link, `NOFOLLOW_LINKS` on an ordinary file must open
normally, a symlinked *directory* mid-path is not the final component).

| VM | Result |
| --- | --- |
| HotSpot 21 | `PASS=19 FAIL=0` |
| CratonVM **baseline** | `PASS=6 FAIL=13` |
| CratonVM **fixed** | `PASS=19 FAIL=0` |

The six baseline passes are exactly the negative controls, so the probe is not
passing vacuously in either direction.

### 3. Regression suite — no fallout

`regression-suite/run.sh` (default CORE set, JDK 17, `TIMEOUT=90`), run on both
binaries. **Identical failure sets**: `RSerial RChannelInterrupt RAtomicArray
RDirectBufferElem RNioNoFollow` — 18 passed, 5 failed on each. The four
non-`RNioNoFollow` failures are pre-existing on this host and unrelated.

The new `RNioNoFollow` vector changed *mode* between the two binaries, which is
the signal:

* baseline — `rc=1`, `java/lang/AssertionError: writeString through a symlink:
  no-exception`: the bug, caught at the first assertion.
* fixed — all 26 checks pass and the class prints `CK RNioNoFollow checks=26`,
  `CK RNioNoFollow refusals=io,io,io,io,io,io,io,io` and `PASS RNioNoFollow` —
  byte-identical to HotSpot's own output for the same class; the harness then
  scores it `rc=124` because the VM does not exit after `main()` returns.

That trailing hang is **not** this fix. It reproduces on a five-line class whose
only content is `FileChannel.open(p, READ)` (`[cratonvm] main() returned; VM
held alive by 2 non-daemon thread(s)`), it does not occur without that call, and
it is why the pre-existing CORE class `RChannelInterrupt` — which also opens a
`FileChannel` — scores `rc=124` on this same host. It is a JDK-17-on-Linux
artifact of this build host, not of the reference environment the suite targets.

### 4. The rewritten write path is byte-exact

`Files.write*` no longer calls `std::fs::write`; it opens through the fd table
and writes via a buffered writer. `RNioNoFollow` therefore also asserts the
three things that change can silently break, and CratonVM matches HotSpot on
all of them: a 4 MiB + 7 byte payload round-trips whole (length **and**
content), a 3-byte write over that file leaves `Files.size() == 3` (truncation,
not a stale tail), and `Files.write(path, List.of("alpha", "beta"))` still
produces exactly `alpha\nbeta\n`.

### 5. Blast radius in the Spring Boot tree

`ApplicationPid` is the **only** place in `core/` or `module/` that passes
`NOFOLLOW_LINKS` as an `OpenOption`, and the only place that passes any
`StandardOpenOption` to a `Files.write*` call. `ApplicationTemp` — the one other
`NOFOLLOW_LINKS` user — uses it purely as a metadata `LinkOption`
(`Files.exists`, `readAttributes`, `isDirectory`, `getOwner`), a path that was
already correct and that this change does not touch;
`ApplicationTempTests` reports `tests=6 failed=0` on HotSpot, on the baseline
and on the fixed binary alike.

## Artifacts

* `native-builtins/src/phases_late/nio_file.rs` — the fix.
* `regression-suite/src/RNioNoFollow.java` + `CORE_CLASSES` in
  `regression-suite/run.sh` — the durable cross-VM vector.
* `probes/NoFollowLinksOpenProbe.java` — the wider probe (19 assertions).
