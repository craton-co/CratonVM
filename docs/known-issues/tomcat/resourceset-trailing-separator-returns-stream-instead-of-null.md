# `*ResourceSet` family — a resource path with a trailing separator should return null, CratonVM returns a stream

| | |
|---|---|
| **Status** | OPEN |
| **HotSpot** | PASS on every class checked |
| **CratonVM** | FAIL, same method, same shared abstract test class |
| **Discovered** | 2026-08-12, complete Tomcat suite rerun on Azure Linux (4 shards) |

## Symptom

8 classes under `org.apache.catalina.webresources` fail, all via the same
inherited test method (`AbstractTestResourceSet.testGetResourceDirWithTrailingFileSeperator`):

- `TestDirResourceSet`
- `TestDirResourceSetInternal`
- `TestDirResourceSetMount`
- `TestDirResourceSetMountTrailing`
- `TestDirResourceSetReadOnly`
- `TestDirResourceSetVirtual`
- `TestFileResourceSet`
- `TestFileResourceSetReadOnly`

```
1) testGetResourceDirWithTrailingFileSeperator(org.apache.catalina.webresources.TestDirResourceSet)
java.lang.AssertionError: expected null, but was:<java.io.FileInputStream@b8>
	at org.apache.catalina.webresources.AbstractTestResourceSet.testGetResourceDirWithTrailingFileSeperator(AbstractTestResourceSet.java:126)
```

The test asks the resource set for a directory's resource path *with a
trailing `/`* and expects `getInputStream()` to return `null` for it (you
can't open an `InputStream` on a directory) — CratonVM instead hands back a
live `FileInputStream`. Same shape, same line, on both `Dir*` and `File*`
resource sets, confirming it's the resource-set base implementation's
trailing-separator handling, not something specific to one backing store.

## Reproduction

```bash
source /data/toolchain/env.sh
cd apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.webresources.TestDirResourceSet
```

HotSpot control: `OK (40 tests)` in 0.16s.

## Suspected root cause (not yet isolated)

Whatever path-normalization CratonVM's `native-io` file-open path does for a
directory path ending in `/` likely strips the trailing separator before
checking `isDirectory()`/opening the stream, where HotSpot's
`FileInputStream`/underlying `open()` treats the trailing-separator form as
still referring to the directory and refuses it (or Tomcat's own resource-set
code checks `path.endsWith("/")` and skips the open, but CratonVM's
equivalent File API returns a different normalized path that no longer ends
in `/` by the time that check runs). Not yet checked against source. No
existing known-issue doc covers this exact signature — checked
`docs/known-issues` and `docs/internal`, only tangential hits on individual
class names from historical full-suite result listings.
