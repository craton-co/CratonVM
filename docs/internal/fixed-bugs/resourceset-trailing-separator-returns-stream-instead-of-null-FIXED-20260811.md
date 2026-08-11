# `*ResourceSet` family — a directory could be opened through `java.io`, so `getInputStream()` handed back a live stream

| | |
|---|---|
| **Status** | FIXED 2026-08-11 |
| **HotSpot** | PASS on every class checked |
| **CratonVM** | PASS — 9 classes, 365 tests, 0 failures |
| **Discovered** | 2026-08-12, complete Tomcat suite rerun on Azure Linux (4 shards) |
| **Fixed in** | `fix/tomcat-resourceset-compositedata-20260811` |

## Symptom as reported

8 classes under `org.apache.catalina.webresources` failing through the same
inherited method,
`AbstractTestResourceSet.testGetResourceDirWithTrailingFileSeperator`:

```
java.lang.AssertionError: expected null, but was:<java.io.FileInputStream@b8>
	at org.apache.catalina.webresources.AbstractTestResourceSet.testGetResourceDirWithTrailingFileSeperator(AbstractTestResourceSet.java:126)
```

## The trailing separator was not the variable

The original page read the trailing `/` as the distinguishing input. It is not.
`java.io.File`'s constructor normalises `new File(base, "d1/")` to the same
path as `new File(base, "d1")`, so by the time Tomcat opens anything the two
tests are asking for the identical file — and both failed. Re-run of the
pre-fix binary on the reported class:

```
1) testGetResourceDirWithTrailingFileSeperator(org.apache.catalina.webresources.TestDirResourceSet)
2) testGetResourceDirWithoutTrailingFileSeperator(org.apache.catalina.webresources.TestDirResourceSet)
Tests run: 40,  Failures: 2
```

The suite listing named only the first of the two. Anyone chasing
trailing-separator normalisation — in Tomcat's `file(name, mustExist)`, in
`RequestUtil.normalize`, in the resource-set base class — would have found
nothing wrong, because nothing there is wrong.

## Root cause

`FileResource.doGetInputStream()` is unconditional:

```java
try {
    return new FileInputStream(resource);
} catch (FileNotFoundException fnfe) {
    return null;   // race: file deleted
}
```

It has no `isDirectory()` guard. It does not need one on HotSpot, because
HotSpot's platform `handleOpen` (`io_util_md.c`) `fstat`s the descriptor it
just opened and converts a directory into `EISDIR`, so the constructor throws
`FileNotFoundException: <path> (Is a directory)`.

Linux's `open(2)` only refuses a directory on the **write** side. CratonVM's
`java.io` lane passed the path straight to `fs::File::open`, which succeeded,
so a live `FileInputStream` on a directory came back and only failed on the
first `read()`. This never showed on Windows: `File::open` there needs
`FILE_FLAG_BACKUP_SEMANTICS` for a directory and fails without it, so the
whole family is Linux-only and Windows-only verification could not have seen
it.

## Three divergences, not one

A differential probe (`FisProbe`, directory / directory-with-separator /
regular file, run against JDK 25 and CratonVM on the same host) found the read
lane was not the only one out of line:

| operation on a directory | HotSpot | CratonVM (pre-fix) |
|---|---|---|
| `new FileInputStream(File\|String)` | `FileNotFoundException: <path> (Is a directory)` | **opened** |
| `new RandomAccessFile(f, "r")` | `FileNotFoundException: <path> (Is a directory)` | **opened** |
| `new FileOutputStream(File)` | `FileNotFoundException: <path> (Is a directory)` | `IOException: Is a directory (os error 21)` |
| `Files.newInputStream(dir)` | opened | opened |

`FileOutputStream` failed with the wrong exception *type*: code catching
`FileNotFoundException` specifically — which the JDK's own signature invites —
would not have caught it.

`Files.newInputStream` is the control, and it is why the check could not go in
`FileDescriptorTable::open_read`. HotSpot's `UnixChannelFactory` opens a
directory read-only without complaint and only fails on the first read;
putting the check in the shared fd-table primitive would have broken a lane
that already matched.

## Fix

`reject_directory_open` in `native-io/src/lib.rs`, called at the eight
`java.io` open sites:

* `fis_open_path` — covers `FileInputStream.open0`, `<init>(String)`,
  `<init>(File)`;
* the four `FileOutputStream` constructors, one of which is also registered as
  `open0(String,Z)`;
* both synthetic `RandomAccessFile` constructors;
* `native_open0` in `native-io/src/random_access_file.rs` — the one that
  actually runs, since `real_raf_enabled()` is the default and the synthetic
  `<init>` natives above are not registered.

That last site is worth remembering: patching the two synthetic RAF
constructors looked complete and changed nothing, because the default build
runs the real JDK `RandomAccessFile` bytecode straight into the platform
`open0` primitive in a different file.

`RuntimeError::FileNotFoundException`'s `path` payload *is* the Java exception
message (`types/src/error.rs`), so the helper builds HotSpot's
`<path> (Is a directory)` suffix into it rather than leaving the message a
bare path.

The stat happens before the open rather than on the resulting descriptor —
the fd table hands back an `FdId`, not a `File`. Syscall count is unchanged
against HotSpot (stat+open here, open+fstat there).

## Verification

All nine classes on the fixed binary, JDK 25 real-JDK mode, Azure Linux:

```
[rc=0] TestDirResourceSet              :: OK (40 tests)
[rc=0] TestDirResourceSetInternal      :: OK (39 tests)
[rc=0] TestDirResourceSetMount         :: OK (44 tests)
[rc=0] TestDirResourceSetMountTrailing :: OK (44 tests)
[rc=0] TestDirResourceSetReadOnly      :: OK (39 tests)
[rc=0] TestDirResourceSetVirtual       :: OK (39 tests)
[rc=0] TestFileResourceSet             :: OK (40 tests)
[rc=0] TestFileResourceSetReadOnly     :: OK (40 tests)
```

Reproduction, for anyone re-checking:

```bash
source /data/toolchain/env.sh
cd apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.webresources.TestDirResourceSet
```

Regression coverage: `reject_directory_open_rejects_a_directory_with_the_hotspot_message`
and `reject_directory_open_allows_a_regular_file_and_a_missing_path` in
`native-io/src/lib.rs`. The second one exists because rejecting a *missing*
path here would silently turn a "No such file" into "Is a directory".
