# The rest of the abstract `FileSystemProvider` surface — FIXED 2026-08-07

**Status: ✅ FIXED.** Follow-on to
[`bug-h2-files-setattribute-abstract-provider-FIXED-20260807.md`](bug-h2-files-setattribute-abstract-provider-FIXED-20260807.md),
which fixed `Files.setAttribute` and closed with:

> Every abstract method on `java.nio.file.spi.FileSystemProvider` is one
> unregistered native away from the same `AbstractMethodError`, because the
> default-provider object is stamped with the abstract class rather than a
> concrete provider. … the registration list next to
> `readAttributes`/`setAttribute` **is** the provider's method table.

This page is that list being enumerated, plus what enumerating it turned up.
Both fixes landed the same day from two sessions working the same filed doc
(`docs/known-issues/h2/bug-h2-windows-files-setattribute-abstract.md`,
now retired); the merge kept dev's write path and this branch's residuals.

## The instrument

`probes/FilesSweep.java` — 43 `java.nio.file.Files` calls that route through a
provider, one deterministic line each (value on success, exception class name
and message on failure), so a CratonVM run diffs cleanly against a HotSpot run
on the same host. `probes/SetAttrWitness.java` is the narrower pure-JDK witness
the original doc asked for.

The population is enumerable, which is the point: `FileSystemProvider` declares
**18** abstract methods, and the registry knows which are registered. Sweeping
the class rather than the reported method is what turned one bug into five.

## What the sweep found

### 1. `Files.isHidden` — the same defect, still open after the first fix

```
java.lang.AbstractMethodError: method java/nio/file/spi/FileSystemProvider.isHidden(
    Ljava/nio/file/Path;)Z has no Code attribute
```

On Windows **and** Linux. `isHidden` is abstract on the provider and was the
only *other* abstract method with no `java/nio/file/Files`-level native standing
in front of it — which is exactly why it, and only it, was reachable.

### 2. `createDirectory` / `delete` / `copy` / `move` — abstract, unregistered, invisible

All four are abstract on the provider and were unregistered too. Their
`Files`-level natives hide that: `Files.delete` never reaches the provider, so
the missing provider method cannot fail. It stops being invisible the moment
anything holds a `FileSystemProvider` and calls it — NIO code written against
the SPI, and every `AbstractFileSystemProvider` caller inside the JDK, do
exactly that.

Registered as delegations to those same `Files` natives, one body per operation.
Not re-implemented: the `Files.move` native carries a
`FileAlreadyExistsException` contract H2 depends on **by type**, and a second
copy of that logic is a second place for it to drift.

### 3. The DOS flags were written to disk and read back from the filename

`setAttribute(p, "dos:hidden", true)` really set `FILE_ATTRIBUTE_HIDDEN`, and
`getAttribute(p, "dos:hidden")` then answered `false` — because `stat_facts`
derived `hidden` from a leading `.` and hardcoded `system`/`archive` to `false`
at the read site. A write that happened, reported as if it had not. Both sides
now go through `dos_attr_flag` (the real `FILE_ATTRIBUTE_*` bits on Windows, the
dot-file convention elsewhere).

`java.io.File.isHidden()` had the same split and is fixed with it: it applied the
leading-`.` rule on *every* platform, which HotSpot-on-Windows contradicts in
both directions. Measured, not assumed:

| | HotSpot/Windows | CratonVM before | after |
|---|---|---|---|
| `.dotfile` | `false` | `true` | `false` |
| plain name + HIDDEN attribute | `true` | `false` | `true` |

### 4. `checkAccess` ignored its `AccessMode[]`

It answered "does the path exist?" for every mode. That is the *only*
implementation `Files.isReadable` / `isWritable` / `isExecutable` have — their
real JDK bytecode calls straight through — so `Files.isExecutable` said `true`
for a plain 0644 file on Linux where HotSpot says `false`. Each requested mode
now routes through `fs_check_access`, the same `access(2)` call
`File.canWrite()` already uses.

### 5. `Files.delete` discarded every error

The divergence the first write-up flagged as "surfaced, not fixed here":

> On HotSpot/Windows `Files.deleteIfExists` on a read-only file throws
> `AccessDeniedException`; on CratonVM/Windows it deletes the file. … it means
> H2's `AccessDeniedException` recovery branch never runs here, and a program
> relying on read-only protection would be surprised.

`p57_delete_path` was `let _ = std::fs::remove_file(path)`, so "the file is
still there" and "deleted" were indistinguishable to every caller. Two things
were wrong and only one of them is about read-only files:

* **Errors were thrown away.** A missing path, a non-empty directory, a
  permission failure — all returned normally. `Files.delete` /
  `deleteIfExists` now raise `NoSuchFileException` / `AccessDeniedException` /
  `FileSystemException`. `deleteIfExists` still swallows *absence* only, which
  is its whole contract.
* **Rust's `remove_file` is deliberately not `DeleteFileW`.** On Windows it
  CLEARS `FILE_ATTRIBUTE_READONLY` and retries, to give Unix semantics — so
  even once errors were reported, the delete would have succeeded where HotSpot
  refuses. That one case is re-refused by hand.

This is what makes H2's recovery branch reachable:
`FilePathDisk.delete` catches `AccessDeniedException`, calls
`Files.setAttribute(file, "dos:readonly", false)` and retries. Both halves of
that branch were dead before today.

Virtual-filesystem paths (`jarfs`/`memfs` sentinels) stay on the best-effort
route: `symlink_metadata` on an encoded sentinel always ENOENTs, and inventing a
`NoSuchFileException` there would be a new wrong answer.

## Layered onto dev's write path

From the same HotSpot diff, three corrections to `write_named_attribute`:

* **`IllegalArgumentException` wording is HotSpot's own** — `'basic:size' not
  recognized`. `BasicFileAttributeView` raises exactly that string; dev's `… is
  not a recognized attribute` was a paraphrase.
* **A missing path is `NoSuchFileException`.** HotSpot stats the file before
  touching any attribute, so the answer does not depend on which setter would
  have failed first.
* **Values are type-checked against the exact class.** This is not cosmetic:
  every reader behind these arms has a benign fallback —
  `filetime_read_millis` answers `0` for an object with no `value` field,
  `unbox_value` answers `Int` for Integer/Byte/Short/Character as well as
  Boolean, and `posix_permission_bits_from_set` answers `0` when it cannot
  probe. So a wrongly-typed value did not *fail*, it *wrote something*:
  `setAttribute(p, "basic:lastModifiedTime", "x")` set the timestamp to the
  epoch and returned normally, and `setAttribute(p, "posix:permissions", "x")`
  was a `chmod 000`.

  `permissions` cannot use an exact class match (any `Set` implementation is
  legal), so it probes `size()`: a real collection answers it — with `0` when
  empty, which *is* a legitimate `chmod 000` — and a `String` does not have the
  method at all.

> **Reusable shape.** A native that reads a Java value through a helper with a
> defaulting fallback has no failure mode for the wrong input — it has a quiet
> wrong output. Where the JDK would throw, check the type *before* the read;
> do not rely on the read to complain.

## Verification

`FilesSweep` against HotSpot on the same host, before and after:

| Arm | Divergent lines before | after |
|---|---|---|
| Linux | 3 of 38 | **0 of 38 — byte-identical** |
| Windows | 13 of 43 | 3 of 43 |

The three remaining Windows lines are known, and every one of them agrees with
HotSpot on the exception **type** — what differs is a message or a separate
defect:

1. `probeContentType` → `UnsatisfiedLinkError:
   sun/nio/fs/WindowsNativeDispatcher.initIDs()V`. A missing JNI native in the
   Windows registry MIME lookup, not an abstract declaration. Filed as
   `docs/known-issues/nio/bug-files-probecontenttype-windows-nativedispatcher-20260807.md`.
2. The `ClassCastException` message. Both say `class java.lang.String cannot be
   cast to class java.lang.Boolean`; HotSpot then appends `(java.lang.String and
   java.lang.Boolean are in module java.base of loader 'bootstrap')` and
   CratonVM appends `(setting 'dos:readonly')`. Reproducing HotSpot's clause
   would mean asserting module and loader facts the call site has not looked up.
3. A `NoSuchFileException` message rendering the path with `/` where HotSpot
   uses `\` — CratonVM's `Path` stores forward slashes; pre-existing and
   unrelated.

`TfsProbe` — `org.h2.test.unit.TestFileSystem.testFileSystem(String)`, the class
the original doc was filed against, on Windows. **Measured at `e3456d0e5`, this
branch before it merged `origin/dev`** — see "A dev regression this ran into"
below for why that is the honest arm to quote:

| prefix | pristine `dev` | fixed | HotSpot |
|---|---|---|---|
| plain disk | `AbstractMethodError` @ `testSetReadOnly` | OK 18.3 s | OK 3.1 s |
| `nioMapped:` | `AbstractMethodError` | OK 7.7 s | OK 10.0 s |
| `split:nioMapped:` | `AbstractMethodError` | OK 10.0 s | OK 8.4 s |
| `memFS:` | `AbstractMethodError` | OK 9.1 s | OK 1.0 s |
| `async:` | `AbstractMethodError` | OK 87.3 s | OK 22.5 s |
| `memLZF:` | `AbstractMethodError` | OK 35.2 s | OK 3.7 s |
| `nioMemFS:` | `AbstractMethodError` | OK 28.6 s | OK 2.9 s |
| `rec:memFS:` | `AbstractMethodError` | OK 12.9 s | OK 2.1 s |
| `cache:` | `AbstractMethodError` | OK 156.3 s | OK 4.1 s |
| `split:` | `AbstractMethodError` | OK 19.3 s | — |
| `encrypt:0007:` | `AbstractMethodError` | OK 1992 s | — |
| `nioMemLZF:12:` | `AbstractMethodError` | OK 5032 s | — |

Twelve of the thirteen prefixes the class drives, `failed=0` on every one (the
thirteenth, `cache:encrypt:0007:`, is the composition of two that pass). The
wall-clock gaps are a throughput matter, tracked separately — `encrypt:0007:`
and `nioMemLZF:12:` are the two known walls, and they are walls, not hangs: both
completed. The doc this closes was about a class that could not get past its
fourth sub-test on any prefix.
Linux `TfsProbe` (plain disk) also passed at 1.2 s on that arm — it always did,
because H2 takes the POSIX branch there.

### A dev regression this ran into

Merging `origin/dev` (`cf4274fda`) made `TestFileSystem` fail again — one
sub-test *later*, at `testSimple`'s `FileChannel.tryLock`:

```
java.lang.NullPointerException: Cannot invoke
    "sun.nio.ch.FileLockTable.add(java.nio.channels.FileLock)" because "flt" is null
```

It is not this work. Built pristine `dev` at `cf4274fda` on the same host and it
fails identically, then bisected it to a single merge with a pure-JDK probe
(`probes/FileLockTableProbe.java`, no H2 in it): `9ddbc9c61` passes,
`6ba350cdd Merge perf/header-16-and-field-packing-20260806: HEADER_SIZE 24 -> 16`
fails. The probe's *suppressed* exception says what it really is —
`"this.fd" is null` in `FileChannelImpl.implCloseChannel` — so
`sun.nio.ch.FileChannelImpl`'s instance fields are not readable at the offsets
its bytecode reads. Filed as
`docs/known-issues/nio/bug-filechannelimpl-instance-fields-read-null-after-header-16-20260807.md`.

So on this branch's merged tip, `TestFileSystem` gets past `testSetReadOnly` —
which is what this doc and its parent are about — and stops at `testSimple` for
a reason that stops it on pristine `dev` too. Confirmed on **both** platforms at
the merged tip: the stack is `testSimple` → `FileChannel.tryLock` → `flt is
null`, with no `AbstractMethodError` anywhere. The `setAttribute` verification
above is quoted from `e3456d0e5` for that reason, and the `FilesSweep` diffs are
quoted from the merged tip (they do not touch `FileChannel`).

One consequence worth naming, because it changes what the probe prints: with
`Files.delete` now reporting failure, `TfsProbe`'s own cleanup
(`FileUtils.delete(base + "/fs")`) throws `DbException: Cannot delete file` when
a failed run leaves the directory non-empty — instead of silently "succeeding"
and printing `DONE failed=1`. That is HotSpot's behaviour too (`Files.deleteIfExists`
on a non-empty directory is `DirectoryNotEmptyException` there); it only looks
new because the delete used to lie.

Rust gates re-run at the merged tip against pristine `dev` at the same commit:

* vs `cf4274fda`: 31 failing test names on each arm, ratchet counts identical to
  the digit. Four names differ in each direction and all four are known flakes —
  two wall-clock budgets (`t1_gc_pause_budget_100k_objects_under_200ms`,
  `re5_http_request_timeout_bounds_delayed_response_headers`), one port-binding
  test (`t4_8_1_jdwp_listening_transport`), and
  `compact_header::tests::forwarding_ptr_inline_boundary`, which passes in
  isolation on **both** arms and only fails under the suite's own parallelism.
* vs `13d2e01b3` (the final merge base): 26 failing names on this branch, 27 on
  pristine — **the same set, plus one on pristine only**
  (`xnio_worker::tests::t19_7_b_java_mirror_round_trip_through_registry`).
  Ratchets identical: 436 raw lock constructions, 323 test-only public API.

Pristine `dev` at `13d2e01b3` does not compile at all — `remap_datagram_sockets`
declared `&std::collections::HashMap<usize, usize>` where `root_source!` wants
`&cratonvm_types::PointerMap` (`rustc_hash::FxHashMap`), so `cargo build -p
cratonvm-vm` fails with `E0308: expected fn pointer, found fn item`. Repaired on
this branch (signature only, both ends) because it blocks building it, and the
pristine arm above is `13d2e01b3` **plus that one repair** so the two arms are
comparable at all.

Rust gates at `e3456d0e5` against its own pristine base: failing-target sets
**identical** (12 targets, red on dev) and ratchet counts identical to the digit
— 1206 shadowed registrations, 53 kind disagreements, 435 raw lock
constructions, 323 test-only public API entries. Green on that arm:
`cargo test -p cratonvm-native-builtins --lib` 3338 passed / 0 failed,
`--features synthetic-jdk` 3513 passed / 0 failed, `stub_ratchet` 7 passed,
`cratonvm-native-io` 424 passed. (The merged-tip re-run is above, under the dev
regression.)

Build note: the Linux binary is `LTO=thin, codegen-units=16` in its own target
dir — the host was at load 8–19 and fat LTO SIGKILLs there. Legitimate for these
instruments, which compare transcripts against a HotSpot control, so the
optimiser's inlining budget is not part of what they ask. The Windows binary is a
stock fat-LTO release build.

## Known, deliberate divergence

`dos:readonly` on **Linux**. HotSpot stores it as a `user.DOSATTRIB` extended
attribute, so `canWrite()` stays `true`; CratonVM clears the write bits, so
`canWrite()` becomes `false`. The read round-trips correctly either way. This
matches the pre-existing `DosFileAttributeView.setReadOnly` native, which made
that choice explicitly ("`setReadOnly` now really chmods/clears the write
bits"); consistency with it is worth more than xattr fidelity on a view Linux
code has no reason to use. The Windows behaviour — the one H2 needs — is exact.

## Also observed in passing

`java.io.tmpdir`: CratonVM honours `$TMPDIR`, HotSpot-on-Linux does not (it is
`/tmp` regardless). Visible only as the parent directory in the sweep's
`NoSuchFileException` message. Unrelated to this work; recorded so the next
person diffing a transcript on a host with `TMPDIR` set knows why.

## What is still not fixed

The shape itself. The default-provider object is still stamped with
`java/nio/file/spi/FileSystemProvider`, and the registration list is still
standing in for a method table. Every abstract method now has a registration —
that is what this page did — but the *next* one added upstream will need one
too, and nothing fails until someone calls it. The durable fix remains what the
first write-up said: stamp the provider object with a real concrete provider
class.
