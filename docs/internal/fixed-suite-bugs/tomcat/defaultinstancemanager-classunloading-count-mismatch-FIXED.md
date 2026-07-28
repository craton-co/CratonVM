# TestDefaultInstanceManager — class-unloading count mismatch (FIXED)

**Status:** FIXED and retired on 2026-07-14 — but **INCOMPLETE**. The test
recurred (`expected:<8> but was:<9>`) on the 2026-07-28 local Windows full-suite
runs and was closed for real on 2026-07-27; see
[defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md](defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md)
for the two actual root causes. Everything below still holds — those changes are
in `dev` and are load-bearing — but they were not sufficient. Two lessons worth
carrying forward:

- Whether the fix below suffices depends on whether the evicted JSP's `Class`
  mirror had been **promoted to old gen** before the test's single
  `System.gc()`. That is a heap-sizing/GC-count accident, which is why the
  Azure-Linux verification recorded here passed while the Windows suite fixture
  (`-Xmx2g`) failed. A pass on one fixture does not generalize for this test.
- A parked pool thread's per-thread JIT memo caches were independently pinning
  the entire JSP compiler graph — something no amount of class-mirror root
  gating can fix.

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
