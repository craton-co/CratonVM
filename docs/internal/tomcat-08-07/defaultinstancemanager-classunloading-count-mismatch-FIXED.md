# TestDefaultInstanceManager — class-unloading count mismatch (FIXED)

**Status:** FIXED and retired on 2026-07-14.

`org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading` loads
three JSPs with `maxLoadedJsps=2`, forces `System.gc()`, and expects Tomcat's
annotation cache to discard only the evicted first JSP. CratonVM previously
reported either `expected:<8> but was:<9>` (the stale key stayed in the cache)
or, while repairing that path, `expected:<8> but was:<7>` (a live JSP key was
incorrectly reclaimed).

## Resolution

- Explicit `System.gc()` now uses the Generational collector's owner-aware
  non-moving full-GC path, so transient native collection overlays no longer
  globally root a dead JSP compiler graph.
- The full collection processes weak-reference queues safely: a live young
  `ReferenceQueue` receives the same post-GC identity-map proof as its active
  `WeakReference`, so Tomcat's `ManagedConcurrentWeakHashMap.maintain()` sees
  the cleared key.
- Rebuilt class-mirror pins now use post-relocation mirror addresses. This
  preserves the still-live `bug36923.jsp` loader/mirror while allowing only the
  evicted `annotations.jsp` class to unload.
- Real-JDK initialization no longer force-drops bridge natives required by
  JMX, restoring Tomcat startup on the current `dev` runtime.

## Verification

Azure Linux (`victor@20.83.144.174`), fixture
`/data/data/apps/tomcat`, a fresh release binary built from the isolated
worktree, with real JDK and JIT enabled:

```text
org.junit.runner.JUnitCore org.apache.catalina.core.TestDefaultInstanceManager

Time: 7.558

OK (1 test)
```

The diagnostic run also confirmed the intended liveness contract: the evicted
`org/apache/jsp/annotations_jsp` mirror is cleared, while the retained
`bug36923_jsp` and `bug51544_jsp` mirrors remain live.
