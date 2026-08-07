# `sun.nio.ch.FileChannelImpl`'s instance fields read back null since HEADER_SIZE 24 → 16

**Status: OPEN**, found 2026-08-07 on Linux. Bisected to a single `dev` merge.
Not a locking bug — a field-layout one.

## Symptom

```
java.lang.NullPointerException: Cannot invoke
    "sun.nio.ch.FileLockTable.add(java.nio.channels.FileLock)" because "flt" is null
        at sun.nio.ch.FileChannelImpl.tryLock(FileChannelImpl.java:1734)
        at java.nio.channels.FileChannel.tryLock(FileChannel.java:1343)
    Suppressed: java.lang.NullPointerException: Cannot invoke
        "java.io.FileDescriptor.valid()" because "this.fd" is null
            at sun.nio.ch.FileChannelImpl.implCloseChannel(FileChannelImpl.java:187)
```

The suppressed one is the clearer of the two: `this.fd` is null on a channel
that has been reading and writing the file happily. **Instance fields of
`sun.nio.ch.FileChannelImpl` are not readable at the offsets the bytecode
reads.**

`flt` cannot be null for any reason internal to the JDK. `fileLockTable()`
(JDK 25 `FileChannelImpl.java:1651`) is a double-checked lazy init that assigns
the field and then returns it:

```java
private volatile FileLockTable fileLockTable;

private FileLockTable fileLockTable() throws IOException {
    if (fileLockTable == null) {
        synchronized (this) {
            if (fileLockTable == null) {
                fileLockTable = new FileLockTable(this, fd);   // fd is ALSO null here
            }
            ...
    return fileLockTable;
}
```

so a null return means the write and the read did not land on the same slot, or
the write was dropped.

## Bisected

`probes/FileLockTableProbe.java` — pure JDK, no H2, four `tryLock`/`release`
calls on a `RandomAccessFile` channel. Built on the Azure Linux host,
`LTO=thin` (a transcript comparison against a HotSpot control, so the optimiser
budget is not part of what it asks):

| `dev` commit | probe |
|---|---|
| HotSpot 25 (control) | `tryLock -> held`, release, re-acquire, `DONE` |
| `1082eb446` Merge fix/files-setattribute-provider | **passes** |
| `9ddbc9c61` Merge feat/spring-boot-residual-rerun | **passes** |
| `6ba350cdd` Merge perf/header-16-and-field-packing-20260806: HEADER_SIZE 24 → 16 | **NPE, `flt` is null** |
| `cf4274fda` (dev HEAD at time of filing) | **NPE, `flt` is null** |

First-parent range between the last good and the first bad merge is exactly
`6ba350cdd` itself, i.e. the `HEADER_SIZE 24 -> 16` work (`d7965af6a feat:
HEADER_SIZE 24 -> 16`, `8d6e2514d docs(gc): record what landed for 24 -> 16, and
where the plan was wrong`).

## Blast radius

* `org.h2.test.unit.TestFileSystem` fails at `testSimple`
  (`TestFileSystem.java:482`, a `FileChannel.tryLock`) on **every** filesystem
  prefix — the sub-test right after the `testSetReadOnly` one that the retired
  `bug-h2-windows-files-setattribute-abstract` write-up was about. The class
  passed all nine measured prefixes on a branch based at `7e754e1bc` (before
  this merge) and stops here on one based at `cf4274fda`.
* Anything using `FileChannel.lock`/`tryLock`: H2's `SingleFileStore`, file-lock
  based single-instance guards, `FileLock`-mediated cache directories.
* `FileChannelImpl.implCloseChannel` NPEs on close, so the failure is not
  confined to locking — every close of a real `FileChannelImpl` is touching a
  null `fd`.

## Reproducing (seconds)

```
javac -d /tmp/flt probes/FileLockTableProbe.java
cratonvm --java-home <jdk25> -c /tmp/flt FileLockTableProbe
```

HotSpot prints `tryLock -> held` twice; CratonVM at `dev` ≥ `6ba350cdd` prints
`FAILED: … "flt" is null`.

## Next step

`FileChannelImpl` is a real JDK class whose instances CratonVM allocates. The
question is which of the three the 24 → 16 change broke for it:

1. the field-offset table computed for the class (a 16-byte header moves every
   field, and `HEADER_SIZE` appears in more than one place);
2. the allocation size, so the tail fields fall outside the object; or
3. a native that writes `fd`/`threads` by a hardcoded slot index rather than by
   resolved offset — `native-io/src/file_channel.rs` writes this object.

(3) is the one to check first: a native that stamps slot N is exactly what a
header-size change silently repoints, and `FileChannelImpl` is written by
natives on the open path. The probe's suppressed `this.fd is null` says the
damage is already visible before any locking is involved, so start there rather
than in the lock table.
