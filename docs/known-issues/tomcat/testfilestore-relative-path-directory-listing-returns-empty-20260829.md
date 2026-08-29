# `TestFileStore` — session-file directory reads as empty (3 of 5 subtests fail)

## Status
New finding, 2026-08-29. Not previously tracked: `apps/tomcat-suite-runner/baseline.tsv`
lists `org.apache.catalina.session.TestFileStore` as `PASS`. Reproduced on a
quiet single-shard host (not contention — this class finished in 1.4s).
Root cause not confirmed, only a strong hypothesis below.

## Symptom

```
1) getSize(org.apache.catalina.session.TestFileStore)
java.lang.AssertionError: expected:<2> but was:<0>

2) keys(org.apache.catalina.session.TestFileStore)
java.lang.AssertionError: ... (empty array instead of {"tmp1","tmp2"})

3) removeTest(org.apache.catalina.session.TestFileStore)
java.lang.AssertionError: expected:<1> but was:<0>

Tests run: 5, Failures: 3
```

`clear` and `pathTraversalSessionId` pass; every subtest that depends on
`FileStore.getSize()`/`.keys()` correctly counting pre-existing files fails —
CratonVM sees 0 files where 2 exist.

## Mechanism

`TestFileStore.beforeEachTest()` (`apps/tomcat/test/org/apache/catalina/session/TestFileStore.java:61-73`)
creates two plain files directly against the **process CWD**:
```java
private static final File dir = new File("SESS_TEMP");
...
file1 = new File("SESS_TEMP/tmp1.session");
file1.createNewFile();
```

`FileStore.getSize()`/`.keys()` (`apps/tomcat/java/org/apache/catalina/session/FileStore.java:135-186`)
instead resolve the storage directory through `directory()` (line 291-316),
which — since `"SESS_TEMP"` is not absolute — resolves it against
`servletContext.getAttribute(ServletContext.TEMPDIR)`:
```java
File file = new File(this.directory);          // "SESS_TEMP"
if (!file.isAbsolute()) {
    File work = (File) servletContext.getAttribute(ServletContext.TEMPDIR);
    file = new File(work, this.directory);      // work/SESS_TEMP
}
```
then calls `dir.list()` on that resolved path.

So the test writes to `<CWD>/SESS_TEMP/` while `FileStore` reads from
`<TEMPDIR-attribute>/SESS_TEMP/`. Those only coincide if `TEMPDIR` is unset
(the two-arg `File(File, String)` constructor treats a `null` parent as
equivalent to the single-arg form, i.e. falls back to a plain relative path
resolved against CWD) — which is presumably what happens on HotSpot, since the
baseline records `PASS`. The regression is plausibly one of:
- `TesterServletContext`'s `TEMPDIR` attribute resolving to a non-null,
  CratonVM-specific value where HotSpot leaves it unset/null, or
- CratonVM's `File(File, String)` two-arg constructor or `File.list()` on a
  freshly-`mkdir()`'d relative-path directory behaving differently than
  HotSpot's.

Not confirmed which; needs a standalone repro isolating `new File((File) null, "x")`
vs `ServletContext.getAttribute` under CratonVM to tell them apart.

## Not yet done
- Standalone repro of the two candidate mechanisms above.
- Check whether other `FileStore`-adjacent classes (e.g. `TestFileStoreConcurrency`,
  already fixed per `fixed-suite-bugs/CRATONVM_BUGS/BUG-Z-filestore-concurrency-gc-segv.md`)
  share any of this directory-resolution path.

## Repro

```bash
cd apps/tomcat-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <tomcat classpath+args> \
  JUnitRunner org.apache.catalina.session.TestFileStore
```
