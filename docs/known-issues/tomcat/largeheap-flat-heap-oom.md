# `*LargeHeap` classes OOM at the harness's flat `-Xmx2g` — 3 classes

> ⚠️ **PARTIALLY RESOLVED, 2026-07-23.** Bumped to `--Xmx 8g` for a rerun.
> `TestByteChunkLargeHeap`/`TestCharChunkLargeHeap` now PASS on HotSpot —
> but **both now FAIL on CratonVM**, a newly-revealed regression (not yet
> individually triaged). `TestEncryptInterceptorLargeHeap` still fails on
> **both** VMs even at 8g, but with different, narrower symptoms than the
> original flat-`2g` OOM: HotSpot gets a semantic assertion failure (`actual
> array was null`), CratonVM **hard-aborts** (`FATAL: OutOfMemoryError:
> young gen exhausted` on a single ~1GB allocation — the young generation
> doesn't grow/promote for one huge object even with a large `-Xmx`). See
> [regressions-revealed-by-fixture-completion-20260723.md](regressions-revealed-by-fixture-completion-20260723.md)
> for detail on all three.

**Not a CratonVM bug** (the original flat-heap-OOM framing) — the heap-size
diagnosis below is still accurate as far as it went; it just wasn't the
whole story for all three classes.

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
