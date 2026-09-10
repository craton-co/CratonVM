# `NativeLibraryLoaderTest` — missing `module-scoped-classes.tsv` entry, not a VM defect

**Status: FIXED 2026-09-06.**

## Symptom

```
@@TESTFAIL io.netty.util.internal.NativeLibraryLoaderTest testMultipleResourcesWithSameContentInTheClassLoader() FAILED
java.lang.UnsatisfiedLinkError: could not load a native library: test3
@@TESTFAIL io.netty.util.internal.NativeLibraryLoaderTest testSingleResourceInTheClassLoader() FAILED
java.lang.UnsatisfiedLinkError: could not load a native library: test2
@@TESTFAIL io.netty.util.internal.NativeLibraryLoaderTest testMultipleResourcesInTheClassLoader() FAILED
org.opentest4j.AssertionFailedError: Unexpected exception type thrown, expected: <IllegalStateException> but was: <UnsatisfiedLinkError>
```

3 of 5 sub-tests fail, deterministically, collector-independent.

## Root cause

The three failing tests each construct their `URLClassLoader` from a
**relative** path:

```java
URL url1 = new File("src/test/data/NativeLibraryLoader/1").toURI().toURL();
```

This resolves against the JVM process's working directory. Maven always runs
tests with the CWD set to the owning module's root
(`apps/netty/common/`), where `src/test/data/NativeLibraryLoader/{1,2}/...`
exists. `run-netty-suite.sh`'s default flat-classpath mode runs every class
from `apps/netty-suite-runner/`, so the relative path resolved to nothing and
`NativeLibraryLoader.load()` failed with `FileNotFoundException` wrapped as
`UnsatisfiedLinkError` — before CratonVM's native-library-loading path was
ever exercised.

This is the identical mechanism already fixed for the 17
`NativeImageHandlerMetadataTest` classes
(`nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md`):
the harness's `--module-scope` machinery (`module-scoped-classes.tsv` +
generated per-module argfiles) exists precisely to pin a class's working
directory and classpath to what Maven would have used. `NativeLibraryLoaderTest`
was simply never added to that table.

## Fix

Added to `apps/netty-suite-runner/module-scoped-classes.tsv`:

```
io.netty.util.internal.NativeLibraryLoaderTest	common	io.netty	netty-common
```

and generated its argfile (`./gen-module-args.sh netty-common`, requires `mvn`
on `PATH` via `source /data/toolchain/env.sh`).

## Verification

```
mode=on-real jit=on jdk=real gc=default classes=1 recorded=1 wall_seconds=1
module-scope=18 (loaded)
status: PASS=1
```

`PASS` with `--module-scope`, `FAIL` (3/5) without — same binary, same host.

## Note

This fix only takes effect when `run-netty-suite.sh` is invoked with
`--module-scope`. That flag defaults OFF (`USE_MODSCOPE=0`, per the script's
own comment "module scope disabled by default on Windows"), so any future
full-suite run that omits it will see this class (and the 17
`NativeImageHandlerMetadataTest` classes) fail again — not a regression, just
the same harness invocation choice.
