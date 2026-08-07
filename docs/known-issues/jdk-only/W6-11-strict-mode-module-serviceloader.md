# W6-11 — the boot layer's `nameToModule` is empty, so no `ServicesCatalog` can ever be built

**No code changed.** This lane was briefed to fix module `ServiceLoader` under
`--jdk-only` at `regression-suite/src/RJdkModule.java:223/234/242`. The brief's
premise does not survive contact with the logs, and the real defect is one
level below `ServiceLoader`, in a file this lane does not own. Both findings are
recorded here.

## 1. `moduleServices()` has never been measured, in either mode

`RJdkModule.main` runs `descriptor()`, `readabilityAndExports()`,
`encapsulation()`, `resources()`, **then** `moduleServices()`. Every log in
`scratchpad/rjdk{,3,4,5,6}/` — newest `rjdk6`, 2026-08-07 02:42 — dies in
`resources()`:

```
AssertionError: a resource in a non-open package must NOT be readable from another module
    at RJdkModule.resources(RJdkModule.java:198)
```

in **both** `--real-jdk` and `--jdk-only`. `moduleServices()` is never entered.
No observation of `:223`, `:234` or `:242` exists on any arm, so "one of the
last 2 strict failures" is an inference from the descriptor line, not a
measurement.

That `:198` failure was fixed after the newest log was taken —
`native-builtins/src/jboss_jdkspecific.rs` 03:39 (the encapsulation gate,
`native_module_get_resource_as_stream`) and `native-builtins/src/lib.rs` 03:52
(pointing the last registration of the triple at it). Both carry
`NativeKind::Bridge` (`register_jboss_jdkspecific` sets it at
`jboss_jdkspecific.rs:1558`; the `lib.rs:18346` site inherits the sticky
`set_category(Bridge)` from `lib.rs:6771`, restored at `7311` via
`regex_category`), so that fix holds in strict too. The next full run is the
first that can reach `moduleServices()`.

## 2. What the real `ServiceLoader` module path needs

Under `--jdk-only` the `ServiceLoader` natives are gone —
`register_service_loader_natives` (`native-builtins/src/service_loader.rs:3204`)
opens with an explicit `r.set_category(NativeKind::SyntheticStub)` and restores
at `:3273`, and `NativeKind::allowed_in` (`native-api/src/registry.rs:4478`)
refuses exactly that kind at registration. So real `java.util.ServiceLoader`
bytecode runs, and it reads providers from **`jdk.internal.module.ServicesCatalog`
only** — never from a module descriptor directly. Two routes, both verified by
`javap -c` on JDK 25:

| route | reached by | reads |
|---|---|---|
| `ModuleServicesLookupIterator` (`ServiceLoader.load(Class)`, `:223`/`:234`) | `iteratorFor(loader)` | `ServicesCatalog.getServicesCatalogOrNull(loader)` (the per-loader `ClassLoaderValue` `CLV`), then `LANG_ACCESS.layers(loader)` → per-layer catalogs |
| `LayerLookupIterator` (`ServiceLoader.load(ModuleLayer, Class)`, `:242`) | `providers(layer)` | `LANG_ACCESS.getServicesCatalog(layer)` → `ModuleLayer.getServicesCatalog()` |

`ModuleLayer.getServicesCatalog()` **self-populates**: if the field is null it
calls `ServicesCatalog.create()` and loops `nameToModule.values()` calling
`catalog.register(m)`. `ServicesCatalog.register(Module)` reads
`m.getDescriptor().provides()` and, per `Provides`, `service()` / `providers()`
— plain field reads on the real `ModuleDescriptor$Provides` layout, which
`build_provides_set` (`jboss_jdkspecific.rs:1134`) already produces correctly.

## 3. What CratonVM supplies — and the one thing it does not

`ModuleRegistry` has the data and the Java mirror of it is right: the
`provides=[com.cratonvm.jdkonly.svc.Greeter->2]` CK line matches HotSpot in both
modes, and `build_module_descriptor` (`jboss_jdkspecific.rs:1199`) fills
`descriptor.provides` from `NativeContext::module_provides`.

The gap is that **nothing connects those `Module` objects to a catalog**:

* `build_boot_layer` (`jboss_jdkspecific.rs:283-331`) allocates the boot
  `ModuleLayer` with a **freshly created, empty** `HashMap` for `nameToModule`
  (`:304-307`) and never adds to it. `build_module` (`:398`) sets the module's
  `name`, `layer` and `descriptor` but does not insert itself into the layer's
  map. The file says so itself at `:808`: *"`nameToModule` field that our
  synthetic boot layer never populates"*. The only other writer,
  `jboss_jdkspecific.rs:1430`, is `Configuration`, not the layer.
* Nothing anywhere calls `ServicesCatalog.putServicesCatalog(loader, …)` — grep
  for `ServicesCatalog` across the tree returns five sites, none of them a put.
* `LANG_ACCESS.layers(loader)` needs `ModuleLayer`'s own `CLV` loader binding,
  which only real `ModuleLayer.defineModules` writes. CratonVM fabricates layers
  and never runs it.

So under strict, `findServices` is called on either a null catalog or a catalog
built from an empty map. Both `:223` and `:242` should report **zero** module
providers — `greets` `[]`, `types` `[]` — and fail on the list comparison, not
on encapsulation and not with a `ServiceConfigurationError`.

This also means CratonVM's own `native_module_layer_modules`
(`jboss_jdkspecific.rs:820-888`), which builds a catalog and calls the real
`ServicesCatalog.register` per module, is populating from that same empty
`nameToModule` and is therefore a no-op for services in *both* modes.

## 4. The fix, and why this lane did not make it

The populate point is `nameToModule` on the boot layer, plus
`ServicesCatalog.putServicesCatalog(appLoader, catalog)` to mirror what real
`java.lang.Module.defineModules` does. Both live in
`native-builtins/src/jboss_jdkspecific.rs`, which is not this lane's file, and
neither `classloading/src/module.rs` nor
`native-builtins/src/service_loader.rs` can reach them — the Rust-side data is
already complete and already exposed through `NativeContext::module_provides`
and `module_names`, so there is nothing missing on this lane's side of the
boundary to add. Filling `nameToModule` is the single change that unblocks both
routes; `putServicesCatalog` is additionally required for the loader route
because the `layers(loader)` fallback cannot work.

## 5. Encapsulation needs no Rust caller identity here

`EnGreeter` and `FactoryGreeter` sit in `…svc.internal`, which the module
neither exports nor opens, and the wave-4 gate correctly refuses
`Constructor.newInstance` there. Under strict this resolves itself: real
`ServiceLoader.getConstructor` calls `ctor.setAccessible(true)` with
`java.util.ServiceLoader` as the caller, and
`AccessibleObject.checkCanSetAccessible` grants unconditionally when
`callerModule == Object.class.getModule()` — i.e. java.base. The override flag
is then set by the real bytecode, and the wave-4 gate honours `override`. No
Rust native has to synthesise a caller identity, because no Rust native is on
the path.

`FactoryGreeter` is likewise handled by the real `findStaticProviderMethod` /
`ProviderImpl`, including `Provider.type()` = the factory's **return** type
(`:234`), which is why HotSpot answers `[EnGreeter, Greeter]`.

## 6. `:242` under strict

The previous lane left `ServiceLoader.load(ModuleLayer, Class)` unregistered
because a native would have to ignore its layer argument — a fabricated success
for any non-boot layer. **That objection does not apply under `--jdk-only`.**
Real `LayerLookupIterator` walks the layer's own `parents()` stack and reads
each layer's own catalog, so it honours the argument exactly. Leaving it
unregistered is the right call for strict, and `:242` becomes correct for free
as soon as `nameToModule` is populated.

## Falsifier

One run of the verify command on a build that includes the 03:39/03:52 resource
fix:

```
cd regression-suite && <cratonvm> --java-home "<jdk>" --jdk-only \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
```

If it fails at `:223` with `module service providers: []`, §3 is confirmed. Any
other shape — a `ServiceConfigurationError`, a non-empty-but-wrong list, an NPE
inside `ServiceLoader` (which would mean `LANG_ACCESS` is null), or a pass —
refutes it.
