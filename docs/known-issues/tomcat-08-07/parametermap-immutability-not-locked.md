# TestParameterMap — ParameterMap not reporting locked after lock()

**Status:** OPEN. **Severity:** low-medium (correctness of an immutability
guard). **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.util.TestParameterMap.testMapImmutabilityAfterLocked`
fails:
```
1) testMapImmutabilityAfterLocked(org.apache.catalina.util.TestParameterMap)
java.lang.AssertionError: ParameterMap is not locked.
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.apache.catalina.util.TestParameterMap.testMapImmutabilityAfterLocked(TestParameterMap.java:147)
```
Tomcat's `ParameterMap` (backs `ServletRequest.getParameterMap()`) has a
`setLocked(true)`/`isLocked()` mechanism to make the map immutable once
request parameter parsing is complete, guarding against post-parse
mutation. The test locks the map and then asserts `isLocked()` returns
`true` — on CratonVM it reports `false` (or an equivalent falsy/wrong
state), meaning the lock flag isn't sticking, isn't visible to the
subsequent read, or the specific `setLocked`/`isLocked` field pairing this
class uses behaves differently under CratonVM.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.catalina.util.TestParameterMap
```

## Recommendation

Read `org.apache.catalina.util.ParameterMap.setLocked`/`isLocked` and
`TestParameterMap.java` around line 147 for the exact lock/assert sequence.
If the field is a plain `boolean` (not `volatile`/`AtomicBoolean`), check
whether CratonVM's field-write visibility or the specific JIT/interpreter
path this test exercises is losing the write — this pattern (a simple
boolean flag write not being observed by a subsequent same-thread read)
would be worth comparing against other plain-field visibility findings in
this codebase before assuming it's Tomcat-specific.
