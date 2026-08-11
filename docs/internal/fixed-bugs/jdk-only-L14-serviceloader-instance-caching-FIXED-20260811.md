> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkServices` passes in the 53/1 run, and this record's failing arm was the DEFAULT (`--real-jdk`) one, which the corpus also exercises. Entirely in-lane; the only closing section is "Residual risk / what would falsify this", which is a falsifier, not an open item.
>
> Previous location: `docs/known-issues/jdk-only/L14-serviceloader-instance-caching.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `ServiceLoader`'s instance cache was allocated, cleared on `reload()`, and never read

**Status:** FIXED in source 2026-08-06 (lane L14, JDK-only wave 2). Not yet
verified against a binary — see *How to verify*. **Both arms must be checked**,
because the failing arm here is the DEFAULT one.

## The inversion

`regression-suite/src/RJdkServices.java`, three arms:

| arm | result |
| --- | --- |
| HotSpot 25 | exit 0, `PASS RJdkServices (19 checks)` |
| `--jdk-only` (strict) | exit 0, `CK RJdkServices checks=19`, `PASS RJdkServices (19 checks)` |
| `--real-jdk` (default) | **exit 1** |

```
Exception in thread "main" java/lang/AssertionError: a single ServiceLoader caches its instances
	at RJdkServices.main(RJdkServices.java:208)
	at RJdkServices.classPathDiscovery(RJdkServices.java:121)
	at RJdkServices.check(RJdkServices.java:36)
```

Strict passing while the default fails is not a policy gap — it is strict mode
being *right*. `register_service_loader_natives`
(`native-builtins/src/service_loader.rs:2485`) registers the whole
`java.util.ServiceLoader` surface under `NativeKind::SyntheticStub`, and
`SyntheticStub` is the one kind `--jdk-only` refuses. Strict therefore runs the
real `java.util.ServiceLoader` bytecode, which caches correctly. Compatible mode
dispatches our native, which did not. The strict arm is a working oracle for
this exact code path.

## Root cause

`RJdkServices.java:118-121`:

```java
ServiceLoader<Greeting> reloadable = ServiceLoader.load(Greeting.class, loader);
Greeting a = reloadable.iterator().next();
Greeting b = reloadable.iterator().next();
check(a == b, "a single ServiceLoader caches its instances");
```

`java.util.ServiceLoader` is specified to cache: iterating one `ServiceLoader`
twice yields the *same* instances in the same order, and only `reload()`
discards them. `native_sl_iterator`
(`native-builtins/src/service_loader.rs`, the `iterator()` registration at
`:2507`) ran `discover_providers` + `Constructor.newInstance` on **every** call
into a **freshly allocated** `ArrayList`, so `a != b`.

The striking part is that the cache was already there, half-built:

* `initialize_real_service_loader_fields` (`:114-161`) allocates
  `instantiatedProviders` and `loadedProviders` and seeds `loadedAllProviders`
  to `0`;
* `native_sl_reload` (`:163-202`) clears both lists and resets the flag;
* **nothing ever read or wrote either list.**

So `reload()`'s clear was a no-op, and the very next check —
`RJdkServices.java:123`, "reload() must discard the cache" — passed only
because there was no cache to discard. This change is the missing reader and
the missing writer, not a new mechanism.

## What changed

All of it in `native-builtins/src/service_loader.rs`.

1. **Cache reader** at the top of `native_sl_iterator`: if
   `loadedAllProviders != 0` **and** `instantiatedProviders` is a non-empty
   list, return that list's own iterator and do no discovery at all.
2. **Cache writer** at the bottom: clear `instantiatedProviders`, copy the
   freshly instantiated providers into it, set `loadedAllProviders = 1`, and
   hand back the *cache's* iterator — so the first and second iterations agree
   by construction rather than through a copy that could drift. If any step of
   the copy fails, the cache is cleared again and the flag is left at `0`: a
   half-copied cache is never replayed.
3. Two small helpers, `sl_instance_cache` and `sl_cache_is_complete`.
4. `sl_service_name` + `provider_not_found_error`, and an all-or-nothing
   `ServiceConfigurationError` after the instantiation loop — see *The second
   defect* below.

`native_sl_find_first`, `native_sl_spliterator` and `native_sl_for_each` all
drive `native_sl_iterator`, so they inherit the cache for free.
`native_sl_stream` is untouched: it yields `ServiceLoader$Provider` wrappers,
and the real JDK gives no cross-instance identity guarantee between `stream()`
and `iterator()` either (`ProviderImpl.get()` constructs on each call).

### The second defect, which the first was hiding

`classPathDiscovery` aborting at `:121` meant `badProvider()` had **never** run
in the `--real-jdk` arm. It asserts (`:196`) that a descriptor naming a
non-existent class raises `ServiceConfigurationError`; the fixture
(`regression-suite/resources/META-INF/services/RJdkServices$Broken`) names
exactly one class, `RJdkServices$NoSuchProviderClass`, which does not exist. Our
native's `load_provider_class` miss path *skipped the entry silently* and
returned an empty iterator, so fixing only the caching defect would have moved
the failure from `:121` to `:196` rather than removing it.

The JDK raises here from `LazyClassPathLookupIterator.nextProviderClass`, which
catches `ClassNotFoundException` and calls
`fail(service, "Provider " + cn + " not found")` — message only, **no cause**.
That matches the `badProviderCause=none` line HotSpot and the strict arm both
print, and check `:197` accepts `"none"` or `"ClassNotFoundException"`.

This is **narrowed on purpose** to "not one single provider could be built".
Full JDK parity would raise on the first unresolvable entry, and this VM reaches
that arm routinely for reasons the JDK never would: the flat classpath scan in
`discover_providers` unions the descriptors of every jar on the path, so an
optional provider whose class this VM cannot yet load is ordinary, and today
every caller of such a service quietly gets the working subset. Raising there
would convert working partial discovery into a hard failure across the
Spring / Tomcat / Elasticsearch / WildFly suites in one step, and this lane can
neither run nor measure them. When *nothing* resolved there is no subset to
protect, and the caller's alternative is an empty iterator that silently lies
about the descriptor it just read.

## Why the native was fixed rather than dropped

The `Executors` precedent (`native-api/src/registry.rs:5785`, the
`drop_real_layout_synthetic` gate) says: when strict mode passes because it
refuses a synthetic stub, drop the stub in real-JDK mode too and let the real
bytecode run. That was considered first and rejected here, on evidence:

* **The native is not a layout fabrication.** The `Executors` pool factories
  and the `StringReader`/`PipedInputStream`/`EnumSet` drops all target natives
  that write a *synthetic slot shape* onto a real-layout object. This one
  allocates a real `ServiceLoader` and fills its real fields by name; the
  failure was a missing behaviour, not a corrupted object.
* **Dropping it removes discovery sources the real bytecode does not have.**
  `discover_providers` is the only path to four provider sources in Compatible
  mode: JPMS `module-info` `provides` declarations (the file's own comment at
  the `service_providers_from_modules` call names
  `ToolProvider.getSystemJavaCompiler` as the casualty), JBoss Modules'
  `module_service_provider_names`, Elasticsearch's `IMPL-JARS` nested-jar
  layout, and the byte-level descriptor reader that exists specifically because
  the JDK's own `getResources -> URL.openStream -> BufferedReader` chain is
  incomplete in the non-`synthetic-jdk` build (`discover_providers`' header
  comment, WP1.8-narrow). Those are load-bearing for suites this lane cannot
  run.
* **The strict arm does not vouch for them.** It proves the real bytecode
  handles the *class-path* case in `RJdkServices`. It says nothing about the
  embedded-jar, module-graph and JBoss cases, and a drop would apply to all of
  them at once.

If someone later wants the drop, it is one `matches!` arm next to the
`Executors` one — but it needs the Elasticsearch/WildFly/JBoss suites measured
first, not this test.

## How the `synthetic-jdk` path stays intact

Nothing is deleted, no registration changes, and no mode gate is added. The
standing rule (*no synthetic method used by real-jdk or synthetic-jdk may be
deleted*) is not engaged.

The new code keys entirely on **named** fields. On a fabricated synthetic-JDK
`ServiceLoader` that does not declare `instantiatedProviders` /
`loadedAllProviders`, `set_field_by_name` is a no-op and `get_field_by_name`
answers `Int(0)` — so `sl_cache_is_complete` is never true, the cache is never
consulted, and `native_sl_iterator` behaves exactly as it does today. The
`serviceName` fallback in `sl_service_name` is the mirror image: `serviceName`
is a CratonVM-only field that a real JDK 25 `ServiceLoader` does not declare, so
that read fails on the real layout and falls back to `service`/slot 0, the same
pair `discover_providers` already consults.

## How to verify

```sh
cd regression-suite
javac -d out src/RJdkServices.java
cp -r resources/META-INF out/            # run.sh does this; the descriptors must be on the class path

# The arm this lane fixes:
cratonvm --real-jdk -cp out RJdkServices

# The arm that already passed and must KEEP passing (it never dispatches
# these natives, so a regression here would mean something else moved):
cratonvm --jdk-only -cp out RJdkServices
```

Both must end with

```
CK RJdkServices greets=[bonjour, hallo, hello] types=[De, En, Fr]
CK RJdkServices config=[RJdkServices$De, RJdkServices$En, RJdkServices$Fr]
CK RJdkServices badProviderCause=none
CK RJdkServices checks=19
PASS RJdkServices (19 checks)
```

`badProviderCause=none` is the line that proves the second defect is fixed;
`checks=19` proves nothing was skipped.

Provider-discovery smoke test for the `--real-jdk` arm, since the cache now sits
in front of every `iterator()`/`findFirst()`/`forEach()`/`spliterator()` call:

```sh
CRATONVM_DIAG_SERVICELOADER=1 cratonvm --real-jdk -cp out RJdkServices
```

Expect `[SL-DBG] iterator() cache published=true` on the first iteration of a
loader and no `[SL-DBG] iterator() entering loop` for the repeat iteration of
the same loader.

## Residual risk / what would falsify this

`loadedAllProviders` is repurposed. In the real JDK that field guards
`loadedProviders` (the `stream()` Provider-wrapper cache), **not**
`instantiatedProviders` — the real iterator tracks its position by index
instead. Every `ServiceLoader` that reaches this native has the flag written by
this file (`0` at construction, `0` on `reload`, `1` only by the publish step),
and no un-overridden real method sets it, so the meanings cannot collide in
practice. The non-empty-cache requirement in the reader exists to make the
collision harmless if one is ever found: a loader carrying the JDK's meaning of
the flag re-discovers instead of reporting itself empty.

The single observation that would falsify this fix: a `--real-jdk` run where
`ServiceLoader.load(X).iterator()` yields **fewer** providers on the second call
than on the first for the same loader. That would mean the copy into
`instantiatedProviders` is partial while `loadedAllProviders` reached `1`, which
the `copied` flag is written to prevent.
