# `TestWebappClassLoaderMemoryLeak`/`TestWebappClassLoaderExecutorMemoryLeak` — "Timer thread still running", still reproducing; root cause already diagnosed internally (child `Thread` does not inherit parent's context classloader)

## Status

**OPEN, still reproducing 2026-09-05.** This is not a new defect — it was
fully root-caused on 2026-06-23 in an internal-only page,
`BUG-TC0622-webapp-classloader-timer-thread-leak`,
but despite living under a `fixed-suite-bugs/` path its own status header
reads **"Status on CratonVM: FAIL. HotSpot: PASS"** (never marked FIXED), and
today's `dev` tip (`7acc0b27c`) still fails both classes identically. It was
also never surfaced as a public `docs/known-issues/` page, and
`docs/known-issues/tomcat/nonpassed-class-census.md`'s current entry for this
pair says only "Not diagnosed" — this page connects the two: it **is**
diagnosed, just not fixed and not previously public. This page adds no new
investigation beyond citing and reconfirming that internal one.

**Severity:** Medium (per the internal doc: this is a real correctness gap in
Tomcat's webapp-classloader thread-leak prevention on CratonVM, not merely a
test assertion — any thread an application spawns escapes
`clearReferencesThreads` because its context classloader never resolves to
the webapp loader).

## Where this was seen

Fresh `dev`-tip Tomcat build, commit `7acc0b27c`, Azure host, full 640-class
ZGC suite run, `zgc-3gc-20260905/shard-0/results.csv`:

| class | result |
|---|---|
| `org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak` | FAIL, `Tests run: 1, Failures: 1` |
| `org.apache.catalina.loader.TestWebappClassLoaderExecutorMemoryLeak` | FAIL, `Tests run: 1, Failures: 1` |

Both logs show the assertion the internal doc names exactly:

```
1) testTimerThreadLeak(org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak)
java.lang.AssertionError: Timer thread still running
```

(`TestWebappClassLoaderExecutorMemoryLeak`'s corresponding method is
`testTimerThreadLeak` too, same assertion text.)

## Root cause (from the internal doc, reconfirmed here by symptom match — not re-derived)

Per `BUG-TC0622-webapp-classloader-timer-thread-leak.md`: a child `Thread`
created on CratonVM does not inherit the spawning thread's
`contextClassLoader` — the field is left null and
`Thread.getContextClassLoader()` falls back to the app/system loader.
Tomcat's `WebappClassLoaderBase.clearReferencesThreads()` only reflectively
stops a leaked `TimerThread` when `thread.getContextClassLoader() == this`
(the webapp loader); since the leaked timer's CCL resolves to the app loader
instead, the gate never fires and the timer thread is never stopped. The
reflective stop machinery itself was proven to work correctly once reached —
the defect is entirely the CCL-inheritance gap in the synthetic `Thread`
constructor overrides in `native-builtins/src/lib.rs`
(`register_synthetic_overrides`), which set name/priority/target/group but
never copy `Thread.currentThread().getContextClassLoader()` into the child.
See the internal doc for the full standalone-probe evidence (HotSpot:
`child_inherited_CCL == parent CCL` is `true`; CratonVM: `false`, child gets
the app loader) and its two candidate fixes.

## What this run adds

Only reconfirmation that the defect is still present on today's `dev` tip and
still affects both sibling classes identically (the internal doc already
predicted the Executor variant "almost certainly" shares the same cause;
this run's logs show the identical `AssertionError: Timer thread still
running` for both). Both logs also show unrelated
`InaccessibleObjectException: ... does not "opens java.lang"/"opens
java.util"` warnings from Tomcat's own `--add-opens`-detection code earlier
in the run — these are a separate, well-understood JVM-flags limitation (the
harness does not pass `--add-opens`), not the cause of the timer-thread
failure; the internal doc's own reproduction command supplies the
`--add-opens` flags and the defect reproduces regardless.

## Recommendation

Unchanged from the internal doc: on `Thread` construction, inherit
`Thread.currentThread().getContextClassLoader()` into the new thread's
`contextClassLoader` field (falling back to the system CL only when the
parent's own CCL is genuinely null), either in the synthetic ctor overrides
or — preferably per that doc — by no longer shadowing the multi-arg `Thread`
constructors at all, so the real JDK 25 `Thread.<init>` bytecode (which
already does this inheritance) runs end-to-end.

## Related

* `BUG-TC0622-webapp-classloader-timer-thread-leak`
  — full root-cause analysis and reproduction (internal tree, not linked —
  cite by path only, see house convention).
* `docs/known-issues/tomcat/nonpassed-class-census.md` — lists this pair as
  "Class-loader leak detection — 2 ... Not diagnosed"; this page corrects
  that to "diagnosed internally, not yet fixed, now public."
