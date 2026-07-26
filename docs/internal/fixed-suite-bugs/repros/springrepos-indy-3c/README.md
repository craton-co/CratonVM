# SpringRepos layer 3c + 3d — Groovy indy guardWithTest repros

Standalone Java repros (no Groovy/Gradle needed) for two CratonVM bugs found
while fixing `SpringRepositoriesExtensionTests` (Spring Boot buildSrc). Both are
FIXED on branch `fix/springrepos-indy-3c` (`../../../../../native-builtins/src/lang_invoke.rs`).

Run each on HotSpot and CratonVM and compare:

```bash
JH="C:/Program Files/Java/jdk-25"; CV=<cvindy3c.exe>
javac -d . LayeredGuardProbe.java VoidTargetProbe.java CachedGuardProbe.java
"$JH/bin/java" -cp . LayeredGuardProbe          # HotSpot baseline
"$CV" --java-home "$JH" --nojit -cp . LayeredGuardProbe
```

## 3c — `guardWithTest` dropped the receiver from the adapter's `type()`
`LayeredGuardProbe.java` — reproduces the original
`ArrayIndexOutOfBoundsException` in `IndyGuardsFiltersAndSignatures.sameClasses`.

`MethodHandles.guardWithTest(test, target, fallback)` copied the target's RAW
bytecode descriptor (`mh_read_desc`), which omits the receiver for an unbound
virtual target, so the GUARD adapter's `type()` reported one fewer parameter than
the target. Groovy's `Selector.setGuards` reads
`handle.type().parameterArray()` to size the `SAME_CLASSES` collector while
building `classes[]` from the (longer) runtime args → `sameClasses(cs, os)`
indexes past `os`. Pre-fix: `handle.type pc=1`; HotSpot/fixed: `pc=2`.

Fix: `mhs_guard_with_test` chains off the EFFECTIVE type (`mh_type_descriptor`).

## 3d — Object-returning polymorphic invoke of a `void` target underflowed
`VoidTargetProbe.java` — reproduces
`operand-stack underflow at value return in IndyInterface.fromCache` (the next
error revealed once 3c was fixed).

Groovy call sites always return `Object`, but the resolved method
(`addRepositories(Closure)`) is `void`. The guarded handle's effective return
type is `Object` (`L`), but `mh_dispatch` of the void leaf yields `Ok(None)`.
`auto_box_return`'s reference-return arm passed `Ok(None)` straight through →
nothing pushed → `fromCache`'s `areturn` underflowed (VM-fatal).

Fix: `auto_box_return` maps `Ok(None)` → `Ok(Some(null))` for `L`/`[` returns
(HotSpot bakes a void→null filter into the `asType(...→Object)` adapter; our
`asType` is a passthrough).

`CachedGuardProbe.java` exercises the full `fallback()` cached-handle
construction (`asSpreader` + `invokeExact((Object[])args)`) as a smoke test.
