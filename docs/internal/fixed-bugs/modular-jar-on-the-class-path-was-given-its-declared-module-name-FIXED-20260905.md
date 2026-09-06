# A modular jar on the CLASS path gave its classes the module name its `module-info` declares, so `--add-opens …=ALL-UNNAMED` could not reach them

**Date:** 2026-09-05 **Status:** FIXED (`ModuleRegistry::named_module_for_package`)
**Found via:** `org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak` /
`…ExecutorMemoryLeak`, the last two open items of
`BUG-TC0622-webapp-classloader-timer-thread-leak`.

## One-line root cause

`ClassManager::define` asked `ModuleRegistry::module_for_package` which module
owns a class's package, and answered with whatever descriptor the registry
held — including the descriptor of a **modular jar found on the application
CLASS path**. A real JVM ignores a `module-info.class` inside a class-path jar
outright: such a jar is an ordinary jar and its classes are in the **unnamed
module**. Because CratonVM labelled them with the declared module name instead,
`--add-opens java.base/java.util=ALL-UNNAMED` — which is *qualified to the
unnamed module*, see `ALL_UNNAMED_TARGET` — did not apply to them, and every
deep-reflection `setAccessible` from such a class threw
`InaccessibleObjectException`.

## The symptom, and why it was invisible on one host and 1/1 on the other

```
WARNING [main] WebappClassLoaderBase.clearReferencesStopTimerThread
  Failed to terminate TimerThread named [leaked-thread] for web application [ROOT]
java.lang.reflect.InaccessibleObjectException: Unable to make member accessible:
  module java.base does not "opens java.util" to org.apache.tomcat.catalina
  (use --add-opens to grant access)
  at ...WebappClassLoaderBase.clearReferencesStopTimerThread(WebappClassLoaderBase.java:1790)
  at ...WebappClassLoaderBase.clearReferencesThreads(WebappClassLoaderBase.java:1612)
...
java.lang.AssertionError: Timer thread still running
```

`--add-opens java.base/java.util=ALL-UNNAMED` **is** on the command line — both
suite runners pass all four `--add-opens` flags (`run-tomcat-suite.sh:195-198`,
`run-tomcat-suite.ps1:329-332`). The public page this closes wrote the warning
off as "a separate, well-understood JVM-flags limitation (the harness does not
pass `--add-opens`)". Both halves of that sentence were wrong: the harness does
pass them, and this *was* the cause.

**The host split is a CLASSPATH SHAPE difference, not a platform one.** The
Linux suite's classpath (`.suite/cp-linux-fixed.txt`) is built from
`output/build/lib/*.jar`, and `catalina.jar` contains a `module-info.class`
declaring `org.apache.tomcat.catalina`. The Windows suite's classpath
(`.suite/cp.txt`) uses the **exploded** `output/classes` directory, which
carries no `module-info` for the eager scan to register — so the same commit
reproduced 1/1 on Azure and 0/1 on Windows. Rebuilding the Windows classpath
jar-first reproduced it in three seconds:

| binary | classpath | verdict |
|---|---|---|
| dev tip `355659d00` | exploded `output/classes` | PASS |
| dev tip `355659d00` | `output/build/lib/*.jar` first | **FAIL** (the trace above) |
| HotSpot 25.0.3+9 | `output/build/lib/*.jar` first | PASS |

The HotSpot arm is the oracle: identical classpath, identical flags, passes.

## Why the CCL half of TC0622 was NOT the cause any more

`BUG-TC0622` root-caused this pair to a missing parent→child
`contextClassLoader` inheritance, and its fix landed the same day
(`CRATONVM_INHERIT_THREAD_CCL`, `native_thread_start0` in
`native-builtins/src/lang_system.rs`). That fix **works** — measured at dev tip
with a standalone probe:

```
worker_self_CCL == set CCL?        true
timer_CCL == cl?                   true      <- the TC0622 gate now passes
pool_worker_CCL == cl?             true
child_CCL_at_construction == cl?   false     <- HotSpot: true (see §Residual)
```

and the Azure log proves it end to end: `clearReferencesStopTimerThread` is
**entered** ("Failed to terminate TimerThread named [leaked-thread]"), which it
could only be if `thread.getContextClassLoader() == webappLoader`. The test was
red for a second, unrelated reason that had been hidden behind the first.

## The fix

`classloading/src/module.rs` — a new `ModuleRegistry::named_module_for_package`,
which is `module_for_package` minus any module `is_class_path_only` reports:

```rust
pub fn named_module_for_package(&self, pkg: &str) -> Option<&str> {
    let name = self.module_for_package(pkg)?;
    if self.is_class_path_only(name) { return None; }
    Some(name)
}
```

`classloading/src/class_manager.rs` — the class-define site calls it instead of
the raw map, behind `CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE` (default ON, `=0`
restores the old labelling).

Three things this deliberately does **not** change:

* the descriptor stays in the registry — service discovery, `packages_of` and
  labelling still want it, and `ClassManager::new` keeps registering it;
* `--module-path` modules keep their names. `automatic` in this VM is not the
  JPMS automatic-module flag, it is *where the descriptor came from*
  (`is_class_path_only`'s own doc), and `vm_init` re-registers every genuine
  `--module-path` module with `automatic = false` immediately after
  `ClassManager::new`. Platform modules are registered `automatic = false` by
  the boot/ext scan;
* access semantics for class-path jars are unchanged in effect — they were
  already "automatic-module", i.e. unenforced, and an unnamed accessor
  short-circuits to allow in `check_deep_reflection_access`.

**The argument was already in the tree, twice.** `ClassManager::new`'s own
comment says of the app class path: *"those jars are on the class path (not a
module path), so the real JDK puts them in the unnamed module"* — and then only
grants automatic-module ACCESS semantics. `ModuleRegistry::service_providers`
makes the identical argument in the identical words and does filter on it.
Class **membership** was the third surface with the same question and the only
one still reading the raw map.

## Verification

`ModuleRegistry` unit test
`a_class_path_jars_module_info_does_not_name_its_classes_module` asserts all
three halves (descriptor kept, class-path jar unnamed, module-path module
named). `cargo test -p cratonvm-classloading`: 804 + 125 pass, 0 fail.

Paired A/B, same host, same jar-first classpath, `--java-home` real-JDK mode:

| class | dev tip | fixed |
|---|---|---|
| `TestWebappClassLoaderMemoryLeak` | FAIL | **PASS** |
| `TestWebappClassLoaderExecutorMemoryLeak` | FAIL | **PASS** |

## Residual (open, separate, benign so far)

CCL inheritance happens at `start()` on this VM and at **construction** on
HotSpot: `new Thread(r, "x").getContextClassLoader()` read before `start()`
answers the app loader here and the parent's CCL there. Nothing measured
depends on it — every consumer reads the CCL from the running child — but it is
a real divergence, and the preferred long-term shape is still
`BUG-TC0622`'s option 2: stop shadowing the multi-arg `Thread` constructors so
the real JDK `<init>` does the inheritance itself.
