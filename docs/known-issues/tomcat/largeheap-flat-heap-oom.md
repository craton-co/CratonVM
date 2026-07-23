# `*LargeHeap` classes OOM at the harness's flat `-Xmx2g` — 3 classes

**Not a CratonVM bug.** Fails identically on real JDK 25 (HotSpot) in the
same fixture.

## Symptom

```
java.lang.OutOfMemoryError: Java heap space
	at org.apache.catalina.tribes.io.XByteBuffer.<init>(XByteBuffer.java:116)
```
(or equivalent, inside whichever huge-payload allocation the class exercises)

## Affected classes

- `org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap`
- `org.apache.tomcat.util.buf.TestByteChunkLargeHeap`
- `org.apache.tomcat.util.buf.TestCharChunkLargeHeap`

## Root cause

Ant's own `<test>` fileset selector (`build.xml`) only *includes*
`**/*LargeHeap.java` when the `test.includeLargeHeap` property is set, and
when it runs them it applies a bigger per-class `-Xmx` override (`test.xmx`,
set specifically for this class group) rather than the suite's normal heap
size. `apps/tomcat-suite-runner/run-tomcat-suite.sh` applies one flat
`-Xmx`/`--Xmx` (`MAX_HEAP`, default `2g`) to every class — these
intentionally-huge-payload tests need considerably more.

## Fix

Either of:
1. **Exclude `*LargeHeap` classes from the default class list**, matching
   Ant's own default (`test.includeLargeHeap` unset) — simplest, matches
   upstream behavior exactly.
2. **Give the runner a per-class heap override** — e.g. in
   `run-tomcat-suite.sh`, detect `*LargeHeap` in the class name and pass a
   larger `--Xmx`/`-Xmx` (try `6g`–`8g`; check
   `test-profiles.properties.default` for the exact upstream value Ant uses
   for `test.xmx` when these are included).

## Verify

```sh
MAX_HEAP=8g CRATONVM_EXE=<binary> TC_ROOT=/data/data/apps/tomcat \
  bash apps/tomcat-suite-runner/run-tomcat-suite.sh hotspot 0 1 largeheap-verify \
  <(printf 'org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap\norg.apache.tomcat.util.buf.TestByteChunkLargeHeap\norg.apache.tomcat.util.buf.TestCharChunkLargeHeap\n')
```
Confirm PASS under HotSpot at the bumped heap before comparing against
CratonVM.
