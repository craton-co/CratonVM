# `HikariDataSourceConfigurationTests` — a generated `$ProxyN` bound its interface by NAME, so Mockito's mock maker died in the second loader world — FIXED 2026-08-11

**Status: FIXED 2026-08-11.** Supersedes
`hikaridatasourceconfigurationtests-pool-start-hang-20260807.md`, which filed
this class as an unexplained 300 s hang. Re-measured on 2026-08-11 the class
does not hang: it takes 41–72 s on a quiet Windows box against a 300 s budget,
and **8 concurrent lanes of it are 13/13 green, all inside 300 s**. What it did
have was a real, reproducible failure the old page never saw, because it read
the run's two log lines as a stall rather than as a test that had already
progressed past them.

## What was actually wrong

`configureDataSourceClassNameToOverrideUseOfAnEmbeddedDatabase` failed with

```
HikariPool$PoolInitializationException: Failed to initialize pool:
  Could not initialize plugin: interface org.mockito.plugins.MockMaker
Caused by: IllegalStateException: No proxy target found for public abstract boolean
  net.bytebuddy.description.method.ParameterList$ForLoadedExecutable$Executable
    .isInstance(java.lang.Object)
    at JavaDispatcher$ProxiedInvocationHandler.invoke(JavaDispatcher.java:1177)
```

HotSpot: 13/13. CratonVM: 12/13 — and **13/13 when the failing method is run
alone**, which is the whole shape of the bug.

Byte Buddy's `JavaDispatcher` builds a `Map<Method, Dispatcher>` from
`proxyInterface.getMethods()` and then looks the incoming method up inside a
`java.lang.reflect.Proxy` handler. `Method.equals` compares declaring classes
by **identity**, so that lookup can only hit if the method the proxy hands the
handler comes from the same `Class` object the map was built from.

CratonVM's `define_or_get_proxy_class` re-resolved the interfaces from the
emitted class file's `interfaces[]` **by name**. `force_loader_faithful_linking`
only makes `resolve_supertype` *prefer* the defining loader's namespace, and it
falls through to the loader-blind `load_class(name)` when that namespace has no
entry — which is routine, since a child loader's namespace holds only the
copies it defined itself. So a proxy over an interface a child loader merely
*sees* linked the application loader's same-named copy, and everything
downstream inherited the wrong `Class`.

This class mixes `@ClassPathExclusions` / `@ClassPathOverrides` methods (which
run under `ModifiedClassPathClassLoader`) with plain ones, so it has two loader
worlds and two copies of Byte Buddy. The first world's mock maker initialised;
the second one's `JavaDispatcher` looked up a `Method` declared by the first
world's interface and missed.

## The identification the old page got wrong

The old page pinned the stall on `testDataSourceGenericPropertiesOverridden`
from "log ordering plus the exact WARN text". The WARN —
`using dataSourceClassName and ignoring jdbcUrl` — is emitted by
`HikariConfig.validate()` only when the real `dataSourceClassName` **config
field** is set alongside `jdbcUrl`. `testDataSourceGenericPropertiesOverridden`
sets `data-source-properties.dataSourceClassName`, an entry in the generic
`dataSourceProperties` bag, and never sets that field, so it cannot emit the
WARN. The two methods that can are the `configureDataSourceClassName*` pair —
the only two that call `getConnection()` and therefore the only two that start a
pool at all. **A WARN's text names a code path, not a test; check which test can
reach that path before naming one.**

## Fix

Two changes, both needed:

* **`interface_id_overrides` on the proxy define.** `Proxy.newProxyInstance`
  was handed the interface `Class` objects themselves — there is no ambiguity to
  resolve — so the `implements` link is now the caller's own `ClassId` rather
  than a name lookup. The ids are name-deduped first, because
  `emit_proxy_classfile` collapses two loaders' same-named interfaces into one
  `interfaces[]` entry (JVMS §4.1) and the override list is positional against
  it.
* **`<clinit>` takes the owner `Class` off `getInterfaces()`.** The generated
  initialiser used `LDC class <owner name>` before calling `Class.getMethod`,
  which would have re-introduced exactly the same by-name binding one level
  down. `ProxyMethod::iface_root` carries the declared interface a method was
  reached through; `getMethod` searches super-interfaces, so an inherited method
  still resolves through its root. `java/lang/Object` owners keep the `LDC
  class` form — there is only one `Object`.

## Reproducing, without the suite

`probes/ProxyTwoWorldProbe.java`, ~2 s:

```powershell
cratonvm --java-home <jdk25> --Xmx 1g -cp "<out>;<byte-buddy.jar>" `
  ProxyTwoWorldProbe <byte-buddy.jar> `
  'net.bytebuddy.description.method.ParameterList$ForLoadedExecutable$Executable' `
  isolatedFirst
```

It loads the interface in an isolated loader and again on the application
loader, then runs the `JavaDispatcher` primitive in each. Before the fix the
isolated world reports `sameAsIface=false` and three
`equals=false hashEq=true` misses; after it, both worlds bind their own copy and
neither misses. **`isolatedFirst` matters** — with the application loader first
there is only one copy and nothing to mis-bind.

`hashEq=true equals=false` is the signature to look for: `Method.hashCode` is
built from the declaring class's *name* and the method name, so two worlds'
copies hash identically and only `equals` can tell them apart.

## Verified

| check | result |
|---|---|
| `HikariDataSourceConfigurationTests`, quiet box | **13/13**, 72 s (was 12/13) |
| the same class, 8 concurrent lanes, 300 s budget | **8/8 lanes 13/13**, none over budget |
| HotSpot control, same fixture | 13/13, 7.7 s |
| `ProxyTwoWorldProbe`, both orders | `result=OK` (was `MISS`) |
| `cargo test -p cratonvm-classloading -p cratonvm-native-builtins` | 20 suites, 0 failures |

`proxy_gen::clinit_resolves_owner_through_get_interfaces_not_by_name` pins the
emitted `<clinit>` shape: `getInterfaces()`+`AALOAD` per interface-rooted
method, no constant-pool `Class` entry for a super-interface owner, and the
`LDC class` fallback retained for `java/lang/Object`.

## Ruled out on the way, and worth not re-running

* **Not a hang, and not GC pressure.** `--Xmx 8g` fails identically; `--nojit`
  fails identically. Neither the heap nor the JIT is involved.
* **Not `Method` name interning or a generic `Map<Method, …>` defect.**
  `probes/ProxyMethodKeyProbe.java` and `probes/ByteBuddyDispatcherProbe.java`
  both report `OK` on CratonVM in a single world — including
  `nameIdentity=true`, the JDK's `getName() == other.getName()` identity
  assumption. The primitive is sound; only the two-world case breaks.
* **`-Dnet.bytebuddy.generate=true` makes the class pass 13/13.** That is a
  diagnostic, not a fix: it routes `JavaDispatcher` through its generated-class
  path and away from the `Proxy`+`Method`-map path entirely, which is what first
  localised the defect.

## Still open, found on the way

`java.net.URLClassLoader` does **not** isolate a class the application loader
has already loaded: `probes/ChildLoaderIsolationProbe.java` reports
`forNameIsolated=false loadClassIsolated=false` for a child loader over the same
jar, where HotSpot reports `true`. That is a separate defect from this one — it
collapses two worlds into one rather than cross-wiring them — and it did not
affect this class, whose isolation comes from Spring's
`ModifiedClassPathClassLoader` (platform parent, its own URL list) and does
work. Recorded here because the probe already exists.
