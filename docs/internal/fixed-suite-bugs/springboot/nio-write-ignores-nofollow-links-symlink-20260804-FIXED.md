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
| `Files.newByteChannel` — dangling link | additionally at its own missing-file pre-check (below) |
| `Files.newBufferedWriter` | real path via `newOutputStream`; synthetic path via `open_buffered_writer` |

`p57_nofollow_reject` performs the check with an `lstat` (`symlink_metadata`,
which does not resolve the final component) immediately before the open, and
raises the plain `java.io.IOException` HotSpot raises. Verified against OpenJDK
21 on Linux: `UnixChannelFactory` special-cases `ELOOP` under `NOFOLLOW_LINKS`
and throws a bare `IOException` — *not* a `FileSystemException` — with the
message `Too many levels of symbolic links (NOFOLLOW_LINKS specified)` and no
path prefix.

`newByteChannel` needed the check twice. It delegates to `newFileChannel` for
the real work, but first runs its own missing-file pre-check so a absent config
source surfaces as `NoSuchFileException` (frameworks catch that one to treat the
source as optional — SmallRye/Keycloak SRCFG00035). That pre-check uses
`Path::exists()`, a `stat`, which reports a **dangling** symlink as absent — so
without a check ahead of it, a dangling link opened `NOFOLLOW_LINKS` returned
the recoverable `NoSuchFileException` instead of `ELOOP`, and the delegation
that carries the real check was never reached.

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

Three release binaries, all built on the Azure Linux host:

* **baseline** — dev `3db59eb9`, where the work started;
* **fixed / merged** — this branch after merging `origin/dev` `b6f0ecdb`;
* **dev control** — `origin/dev` `b6f0ecdb` on its own, so that the 14 dev
  commits merged in mid-flight cannot be mistaken for this change's fallout.

HotSpot reference as noted per row.

### 1. The reported test — `ApplicationPidTests`

SbRunner, real JDK 25 (`/data/jdk25-real-20260717/jdk-25.0.3+9`),
`core/spring-boot` test classpath.

| VM | Result |
| --- | --- |
| HotSpot 25 | `tests=13 failed=0` |
| CratonVM **baseline** | `tests=13 failed=1` — `whenSymlinkToTargetExistsAtPidFileLocationWriteThrows` |
| CratonVM **fixed** | `tests=13 failed=0` |
| CratonVM **merged** | `tests=13 failed=0` |

`ApplicationTempTests`, the adjacent class, reports `tests=6 failed=0` on all
four.

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
| CratonVM **merged** | `PASS=19 FAIL=0` |

The six baseline passes are exactly the negative controls, so the probe is not
passing vacuously in either direction.

### 3. Regression suite — no fallout, and the branch is fully green

`regression-suite/run.sh`, default CORE set, real JDK 25
(`/data/jdk25-real-20260717/jdk-25.0.3+9`), `TIMEOUT=120`. The control is a
release build of **`origin/dev` at `b6f0ecdb`** — the same commit this branch
was merged with, not the older `3db59eb9` the work started from, so no
intervening dev commit can be mistaken for fallout.

| Binary | Result |
| --- | --- |
| `origin/dev` control | **23 passed, 0 failed** |
| this branch (merged) | **24 passed, 0 failed** |

The one extra class is `RNioNoFollow` itself. Every pre-existing vector holds
its status exactly; this change moves nothing.

The same vector run against the control binary under the same JDK is `rc=1`,
`java/lang/AssertionError: writeString through a symlink: no-exception` — so it
is a genuine discriminator and not green by construction.

> A JDK-17 aside, because the first pass of this validation was run that way and
> the numbers look alarming: on JDK 17 both binaries score 18 passed / 5 failed
> with the *identical* set `RSerial RChannelInterrupt RAtomicArray
> RDirectBufferElem RFileTimes`, and `RNioNoFollow` adds a sixth by hanging
> after it has printed `PASS`. That hang reproduces on a five-line class whose
> only content is `FileChannel.open(p, READ)` (`[cratonvm] main() returned; VM
> held alive by 2 non-daemon thread(s)`) and does not occur without that call,
> which is also why the pre-existing CORE class `RChannelInterrupt` reds there.
> None of it survives on the reference JDK 25. JDK 17 is not the suite's
> reference and these numbers should not be read as a baseline.

### 4. The rewritten write path is byte-exact

`Files.write*` no longer calls `std::fs::write`; it opens through the fd table
and writes via a buffered writer. `RNioNoFollow` therefore also asserts the
three things that change can silently break, and CratonVM matches HotSpot on
all of them: a 4 MiB + 7 byte payload round-trips whole (length **and**
content), a 3-byte write over that file leaves `Files.size() == 3` (truncation,
not a stale tail), and `Files.write(path, List.of("alpha", "beta"))` still
produces exactly `alpha\nbeta\n`.

All 27 of the vector's checks pass, and the class's full stdout —
`CK RNioNoFollow danglingReadOnly=java.io.IOException`,
`CK RNioNoFollow checks=27`,
`CK RNioNoFollow refusals=io,io,io,io,io,io,io,io`, `PASS RNioNoFollow` — is
byte-identical to HotSpot's for the same class.

### 4a. Crate tests

`cargo test --release -p cratonvm-native-builtins`: **3250 passed, 0 failed**
(lib) plus `aes_gcm_kat` 5/5, `registry_contracts` 5/5,
`shim_inheritance_guard` 3/3 and `stub_ratchet` 4/4 — 0 failed anywhere.

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
