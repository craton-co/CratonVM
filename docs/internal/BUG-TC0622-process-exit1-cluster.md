# Bug TC0622 — process exit(1) cluster: per-loader class identity ignored on `defineClass` (weaving) + cross-context session-id divergence

> **Root cause (primary, weaving):** CratonVM resolves loaded classes by **name
> across ALL class loaders** — `ClassManager::get_loaded_class_id(name)` walks the
> bootstrap/extension/application delegation chain *and* every user-defined loader
> and returns the **first** `ClassId` found for that name. So once any loader has
> defined `org/apache/catalina/loader/TesterUnweavedClass`, a *different*
> `WebappClassLoaderBase` that calls `defineClass(name, weavedBytes, …)` gets back
> the **already-cached class (with the original bytes)** instead of a fresh,
> per-loader class built from the supplied bytes. This violates the JVMS §5.3
> rule that class identity is `(defining loader, name)` — CratonVM keys it on
> `name` alone. The `ClassFileTransformer` weaving therefore has no observable
> effect, and 7/10 weaving tests fail their `assertEquals`.

* **Severity:** High (core class-loader identity / instrumentation correctness; affects any framework that relies on per-loader class isolation or load-time weaving — Tomcat webapps, AOP/instrumentation agents, OSGi-style isolation).
* **Status:** **Sub-cause A (weaving) → ✅ FIXED + MERGED to dev** (commit `e92fe438`, 2026-06-22; surgical reflection-path fix — reflective construct/dispatch now honor the mirror's exact `ClassId` per JVMS §5.3 instead of re-resolving by name). Validated on dev `0284432c`: `TestWebappClassLoaderWeaving` OK (10 tests, was 7 failures); `scratch_weave/WeaveRepro` + `ReflectSanity` PASS ==HotSpot. **Sub-cause B (session-id) → HANDOFF** (separate, higher-level connector/session behavior; still OPEN, not investigated to root).
* **Run date:** 2026-06-22
* **Binary:** dev df11ac00 (worktree `C:\craton\CratonVM-tctest`), exe `C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe`

## Affected classes

| Class | Result | Sub-cause |
|---|---|---|
| `org.apache.catalina.loader.TestWebappClassLoaderWeaving` | 7 of 10 fail | **A** — per-loader identity ignored on `defineClass` |
| `org.apache.catalina.connector.TestValidateClientSessionId` | 1 of 2 fail | **B** — cross-context session-id divergence (separate) |

The `[cratonvm] System.exit(1) called — process terminating` line at the end of
both captured logs is **NOT a VM abort, panic, or unsupported-op**. Both runs
complete a fully normal JUnit run and print the regular `FAILURES!!! Tests run:
N, Failures: M` summary; `org.junit.runner.JUnitCore.main` then calls
`System.exit(1)` *because tests failed* (standard JUnit behavior). The exit-1 is
a real test failure, not a crash. The real defects are why the `assertEquals`
comparisons fail on CratonVM but pass on HotSpot.

---

## Sub-cause A — Weaving (PRIMARY, FIX)

### Symptom

`.log` summary:

```
..E.E.E.E.E...E.E
There were 7 failures:
1) testAddedTransformerInstrumentsClass1   2) testAddedTransformerInstrumentsClass2
3) testCopiedClassLoaderExcludesResourcesAndTransformers
4) testTransformersExecuteInOrderAdded1    5) testTransformersExecuteInOrderAdded2
6) testRemovedTransformerNoLongerInstruments2  7) testRemovedTransformerNoLongerInstruments3
FAILURES!!!  Tests run: 10,  Failures: 7
```

Every failure is the **same** assertion:

```
org.junit.ComparisonFailure: The second result is not correct.
    at org.apache.catalina.loader.TestWebappClassLoaderWeaving.testAddedTransformerInstrumentsClass1(...:143)
```

i.e. line 143 `assertEquals("Hello, Weaver #1!", result)` got the unweaved
`"Hello, Unweaved World!"`. The 3 **passing** tests (`testNoWeaving`,
`testAddingNullTransformerThrowsException`, `testRemovedTransformerNoLongerInstruments1`)
are exactly the ones where **no active transformer should change** the class —
so the symptom is "weaving never takes effect; the class always resolves to
whichever version was defined first by name."

The transformer is correctly *registered* (log: `Added class file transformer
[…$ReplacementTransformer@…] to web application [weaving]`) and Tomcat's pure-Java
`WebappClassLoaderBase.findClassInternal` does run the transformer loop and call
`defineClass(name, transformedBytes, 0, len, CodeSource)`. The bytes reach
CratonVM's `defineClass` native — the loss happens **inside** the VM's loaded-class
resolution.

### Root cause

`WebappClassLoaderBase.findClass(...)` (real Tomcat Java) →
`ClassLoader.defineClass(String,byte[],int,int,…)` → CratonVM native
`cl_define_class_basic` (`native-builtins/src/classloader.rs:1166`) →
`Vm::define_class_full` (`vm/src/vm/vm_exec.rs:5923`) →
`ClassManager::define_class_with_options` (`classloading/src/class_manager.rs:2434`).

The define path itself is fine: it assigns each `WebappClassLoaderBase` a unique
`loader_id` (`get_or_assign_loader_id`, classloader.rs:462) and the duplicate-define
probe is correctly keyed on `(loader_id, name)`. The bug is in **resolution**:

`ClassManager::get_loaded_class_id(name)` (`classloading/src/class_manager.rs:1581`)
looks up a class by **name only**, returning the first hit across the entire
delegation chain *plus every user loader*:

```rust
pub fn get_loaded_class_id(&self, name: &str) -> Option<ClassId> {
    for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {           // Bootstrap, Ext, App
        if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
            return Some(id);
        }
    }
    if !self.user_loaders.is_empty() {
        for loader_id in &self.user_loaders {                    // EVERY user loader
            if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
                return Some(id);
            }
        }
    }
    None
}
```

This is also the fast path of `load_class` (`class_manager.rs:2271-2308`): "if a
class with this name is loaded by *any* loader, return it." `TesterUnweavedClass`
is on the application classpath (`output/testclasses` ∈ cp), so the first define
of that name — under whichever loader gets there first — wins permanently. Every
subsequent name-based resolution (including the reflective `newInstance()` /
`Method.invoke()` the test does on the returned mirror) collapses to that first
`ClassId`, so the weaved bytes are silently discarded. Class identity in CratonVM
is keyed on **name**, not on **(defining loader, name)** as JVMS §5.3 requires.

### Reproduction (minimal, no Tomcat)

`scratch_weave/WeaveRepro.java` (compiled with JDK 25, run from `apps/tomcat`):
three trivial `ClassLoader` subclasses each call `defineClass("…TesterUnweavedClass", bytes, 0, len)`.

```
=== HotSpot (jdk-25) ===
CASE A (fresh loader, weaved bytes): Hello, Weaver #1!       [expect: Hello, Weaver #1!]
CASE B (fresh loader, orig bytes):   Hello, Unweaved World!  [expect: Hello, Unweaved World!]
CASE C (fresh loader, weaved again): Hello, Weaver #1!       [expect: Hello, Weaver #1!]

=== CratonVM ===
CASE A (fresh loader, weaved bytes): Hello, Weaver #1!       [expect: Hello, Weaver #1!]
CASE B (fresh loader, orig bytes):   Hello, Weaver #1!  ***  [expect: Hello, Unweaved World!]   <-- WRONG
CASE C (fresh loader, weaved again): Hello, Weaver #1!       [expect: Hello, Weaver #1!]
```

CASE B is the bug: a **different** loader defining the **original** bytes gets
back the **weaved** class cached under CASE A's loader. `WeaveRepro2.java` (define
orig first, then weaved under a fresh loader) shows the mirror image: the weaved
define returns `Hello, Unweaved World!` (the first-cached version). HotSpot keeps
each loader's definition distinct; CratonVM does not.

Full-class repro (matches captured logs exactly — 7 failures):

```powershell
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = Get-Content C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt -Raw
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
Set-Location C:\craton\CratonVM\apps\tomcat
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore `
    org.apache.catalina.loader.TestWebappClassLoaderWeaving
```

### Recommendation — FIX

Key loaded-class identity on **(defining loader, name)**, not name alone:

* `get_loaded_class_id` / the `load_class` fast path must probe the **requesting
  loader's** id first and only fall back to parent delegation per JVMS §5.3 — it
  must not return a class an *unrelated* user loader happens to have defined under
  the same name. A user-loader `defineClass` (with no parent delegation) must
  always create a fresh, loader-private `ClassId` from the supplied bytes.
* The plumbing already threads a real per-loader `loader_id` end to end
  (`get_or_assign_loader_id` → `define_class_full` → `define_class_with_options`),
  and the dup-probe is already correctly `(loader_id, name)`-keyed — so the fix is
  localized to the **name-only lookup helpers**, which currently flatten the
  loader dimension. Be careful that bootstrap/platform classes (java.*, etc.)
  must still resolve once and be shared.

This is a foundational class-loader-isolation correctness fix; it likely also
helps other webapp-isolation / instrumentation / hot-redeploy scenarios in the
gauntlet, so validate broadly (full classloading test suite) after the change.

---

## Sub-cause B — Session-id across contexts (SEPARATE, HANDOFF)

### Symptom

`.log` summary: `..E  Tests run: 2, Failures: 1`. Passing:
`testMaliciousSessionIdRejected`. Failing: `testValidSessionIdAcceptedAcrossContexts`.

```
1) testValidSessionIdAcceptedAcrossContexts(...)
org.junit.ComparisonFailure:
    at org.junit.Assert.assertEquals(Assert.java:117)
    ... (JUnitCore frames only; no app frame in the captured trace)
[cratonvm] System.exit(1) called — process terminating
```

The failing assertion is `TestValidateClientSessionId.java:86`
`assertEquals(sessionId1, sessionId2)`: a session created under context `/app1`
(cookie path `/`) must be **accepted and reused** when the same `JSESSIONID`
cookie is replayed against `/app2`. On CratonVM the two session ids differ (the
cookie is not honored across contexts), so the equality fails.

The full request lifecycle runs (two Tomcat contexts start, the HTTP round-trips
complete — see the connector start/stop INFO lines and the benign
`Illegal access: this web application instance has been stopped` /
`stop() … already been called` shutdown noise), so this is a **functional/behavioral
divergence in the connector + session-cookie path**, not a VM core defect and not
related to Sub-cause A.

### Root cause

Not pinned. Candidates (run uses `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1`):
cross-context `JSESSIONID` cookie matching / `sessionCookiePath="/"` handling,
the request-cookie parse, or session-manager lookup returning a fresh session
instead of reusing the supplied id. Needs a focused HTTP-level repro
(`getUrl` with a replayed `Cookie: JSESSIONID=…`) comparing CratonVM vs HotSpot
to localize whether the cookie reaches the request, is matched to an existing
session, and whether the session manager is shared/validated across contexts.

### Recommendation — HANDOFF

Separate, lower-severity connector/session investigation. Document and triage on
its own; it does not block or interact with the Sub-cause A fix. (Both sub-causes
only share the cosmetic `System.exit(1)` symptom, which is just JUnitCore's
non-zero exit on any failure.)

---

## Notes

* `System.exit(1)` here is **normal JUnit**, not a CratonVM abort — confirmed by
  the regular `FAILURES!!! Tests run/Failures` summary preceding it in both logs.
* Repro scaffolding left at `apps/tomcat/scratch_weave/` (`WeaveRepro.java`,
  `WeaveRepro2.java`) — minimal, Tomcat-free demonstrations of Sub-cause A.
