# `TestDefaultInstanceManager.testClassUnloading` — off-by-one unloaded-class count

**Status:** OPEN, well-isolated. Confirmed CratonVM-only regression —
passes on HotSpot; fast and deterministic (~22-26s, reproduced identically
across two runs on different `dev` tips).

## Symptom

```
java.lang.AssertionError: expected:<8> but was:<9>
	at org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading(TestDefaultInstanceManager.java:66)
```

One extra class remains loaded/counted where the test expects exactly 8 to
have been unloaded (or is being double-counted) after
`DefaultInstanceManager`-driven class unloading. HotSpot gets exactly 8.

## Analysis

Likely one of:
- an extra class incidentally loaded by CratonVM's own runtime machinery
  during the test's webapp classloader lifecycle (something HotSpot doesn't
  need to load in the same scenario), inflating the "still loaded" count by
  one, or
- a genuine double-count in whatever CratonVM-side bookkeeping the test's
  class-unloading detection relies on (e.g. a `WeakReference`/`PhantomReference`
  queue drained twice for one entry).

Not root-caused to the exact class or counting mechanism in this session —
read `TestDefaultInstanceManager.java`'s `testClassUnloading` method and
whatever helper it uses to count unloaded classes (likely a
`ClassLoaderReference`-queue poll or a WeakHashMap size check) as the next
step, then compare which specific class differs between the CratonVM and
HotSpot runs (e.g. via `-verbose:class` / `--verbose:class` diffing).

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 120 -Parallel 1 -RunName definstmgr-repro -Exe <cratonvm.exe>
```
