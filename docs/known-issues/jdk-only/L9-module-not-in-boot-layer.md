# `--module-path` / `--add-modules` were parsed and then read by nobody

**Status:** PARTIAL (resolution layer landed 2026-08-06 in
`classloading/src/module.rs`, unverified — no binary was built in the session
that wrote it; the two wiring patches are recorded below and were NOT applied
by this lane, which does not own those files). Lane L9 of the jdk-wave2 pool.

## The failure

`regression-suite/src/RJdkModule.java` fails in **both** `--real-jdk` and
`--jdk-only`. HotSpot 25 **passes** it — 44 checks, exit 0. (An earlier
orchestrator run recorded this vector as "HotSpot fails it too, bad vector".
That was a harness error: the ad-hoc HotSpot arm passed the wrong module name
and got `FindException: Module cratonvm.regression.jdkonly not found`. The
captured `scratchpad/rjdk/RJdkModule.hs` still shows that stale invocation —
ignore it.)

```
Exception in thread "main" java/lang/AssertionError: module must be in the boot layer
    at RJdkModule.main(RJdkModule.java:247)
    at RJdkModule.descriptor(RJdkModule.java:57)
    at RJdkModule.check(RJdkModule.java:42)
```

No `CK RJdkModule ...` line is emitted at all: we fail on the **4th** check of
the first section, so essentially nothing about module support is exercised.

## What is asserted

`RJdkModule` runs from the **class path** (the unnamed module) and consumes a
real named module resolved from `--module-path`:

```java
static Module svc() {                                          // line 46
    Optional<Module> m = ModuleLayer.boot().findModule(MODULE); //  1
    check(m.isPresent(), ...);
    return m.get();
}
static void descriptor() {
    Module m = svc();
    check(m.isNamed(), ...);                                   //  2
    check(m.getName().equals(MODULE), ...);                    //  3
    check(m.getLayer() == ModuleLayer.boot(), ...);            //  4  <-- fails
    ...
}
```

Then, across 44 checks: the descriptor's `exports`/`opens`/`packages`/
`provides`/`requires` sets, unnamed-vs-named readability, `isExported`/`isOpen`
per package, class-to-module identity, the app-loader identity of module-path
classes, deep-reflection encapsulation (opened vs exported-but-not-opened vs
fully encapsulated), module resource encapsulation, and module-path
`ServiceLoader` providers including the `provider()` static-factory form.

## Root cause — it is (c), *parsed then ignored*, plus a second independent bug

Both candidate (c) and candidate (b) from the lane brief are real, and (c) is
the one that starves everything else.

### (c) — the flags never reach the VM

`vm-cli/src/main.rs` parses both flags and stores them:

```
vm-cli/src/main.rs:3428:  config.module_path = VmConfig::parse_classpath(mp);
vm-cli/src/main.rs:3449:  config.add_modules = args.add_modules.clone();
```

and `grep -rn '\.module_path\b|\.add_modules\b' --include=*.rs` over the whole
workspace returns **exactly those two lines** (plus the `VmConfig` field
declarations and their `Default` initialisers at `vm/src/config.rs:486/501/747/751`;
the `jboss_module_loader.rs` hits are JBoss's own unrelated `-mp` scanner).
Nothing reads either field. The `--add-opens was parsed then ignored` precedent
in this repo is the same disease, in the same launcher, on the adjacent flag.

Consequence: the module's classes are on no search path, and no
`ModuleDescriptor` for `cratonvm.jdkonly.svc` is ever registered in
`ClassManager::module_registry` — `ClassManager::new` only scans
boot/ext/**app class path** for `module-info.class`
(`classloading/src/class_manager.rs:2630-2638`).

### (b) is real too, and it is what the stack trace actually names

Check 1 (`m.isPresent()`) **passed** even though nothing had been resolved,
because `ModuleLayer.findModule` fabricates a `Module` for any syntactically
valid name:

```rust
// native-builtins/src/jboss_jdkspecific.rs:436
let module = build_module(ctx, &name, layer_ref);
let opt = wrap_optional_present(ctx, module);
```

There is no lookup and no `Optional.empty()` path. `findModule("no.such.module")`
answers `Optional.of(...)` today. So the vector's *first* check — the one whose
message names `--module-path` — cannot fire, and the failure surfaces three
checks later.

Check 4 (`m.getLayer() == ModuleLayer.boot()`) then fails on **object
identity**: `build_boot_layer` (`jboss_jdkspecific.rs:261`) allocates a fresh
`java/lang/ModuleLayer` on *every* call despite its callers' doc comments
saying "the cached boot layer". `ModuleLayer.boot()` is spec'd to be a
singleton; here `ModuleLayer.boot() != ModuleLayer.boot()`. That is a
standalone defect, independent of the module path, and it also breaks the
layer-scoped `ServiceLoader.load(ModuleLayer.boot(), Greeter.class)` at
`RJdkModule.java:238`.

Candidate (a) — "parses but never builds a boot layer" — is ruled out in that
narrow form: a boot layer object *is* produced. It is simply produced fresh
each time and populated from nothing.

## What changed

### Landed: `classloading/src/module.rs` (this lane owns it)

A `--module-path` / `--add-modules` resolution layer, all pure of VM state:

* `ModulePathModule { root, descriptor, packages }` — one named module found
  on the module path.
* `parse_module_info(bytes) -> Option<(ModuleDescriptor, Vec<String>)>` — the
  public, IO-free half of `ClassManager::try_register_module_info`.
* `exploded_packages(dir)` / `modular_jar_module(jar)` — derive the package set
  when `ModulePackages` is absent. It **is** absent here: `javap -v
  regression-suite/build-modules/cratonvm.jdkonly.svc/module-info.class` shows
  the attribute list is `SourceFile` + `Module` and nothing else, because
  `javac` only emits `ModulePackages` via `jar`/`jlink`. Without the tree walk
  `d.packages()` is empty and no class maps back to the module.
  Directories that are not legal package components (`META-INF`) are dropped,
  matching `jdk.internal.module.ModulePath`.
* `scan_module_path(entries)` — accepts both spellings `java` accepts: an entry
  that *is* a module root (exploded dir with `module-info.class`, or a modular
  JAR), and an entry that is a directory *of* module roots. Child order is
  sorted so the resulting search-path order is stable across runs and OSes.
* `resolve_module_path(entries, add_modules)` — the `--add-modules` root set
  plus its transitive non-`static` `requires` closure, restricted to what is
  observable; `ALL-MODULE-PATH` selects everything; empty module path returns
  empty with no filesystem probing, so an unconditional call at VM init is a
  no-op for a plain `-cp` launch.

Every descriptor it returns has `automatic = false`. That is the point of a
module path: a jar on the *class* path gets automatic-module semantics
(`ModuleDescriptor::exports_package_to` / `opens_package_to` short-circuit to
`true`), and a module resolved from a *module* path must not.

Two unit tests were added: `package_segment_rejects_non_identifiers` and
`empty_module_path_resolves_nothing`.

### Not landed — two wiring patches this lane does not own

Both are recorded verbatim in the lane report. In summary:

1. **`vm/src/vm/vm_init.rs:982`** — call `resolve_module_path`, append each
   resolved module's root to the application class path handed to
   `ClassManager::new` (the real JDK also defines module-path classes to the
   **application** loader — `RJdkModule.java:131` asserts exactly that), then
   re-`register` each descriptor into `class_manager.module_registry` and
   rebuild the readability graph. The re-register is what flips the entry from
   the `automatic = true` the app-class-path scan gave it to the explicit
   module it actually is.
2. **`native-builtins/src/jboss_jdkspecific.rs:261`** — memoise the boot
   `ModuleLayer` per `ctx.vm_identity()` behind a JNI global root
   (`add_global_root` / `resolve_global_root`), so `ModuleLayer.boot()` is one
   object per VM. Keyed by `vm_identity` because Rust tests build several `Vm`s
   in one process and a process-global object cache goes stale across VM
   lifetimes.

## What remains — the checks behind this one

We currently reach 0 of 44. Even with both patches above, these are known to
still fail, each verified against the source rather than guessed:

1. **`Module.getDescriptor()` returns an empty descriptor.**
   `native-builtins/src/lib.rs:30398 build_synthetic_module_descriptor` sets
   `requires`, `exports`, `opens`, `provides` **and** `packages` to empty sets
   unconditionally; only `name`, `open`, `automatic` and `uses` carry data.
   Trips `RJdkModule.java:69` (exports), `:78` (opens), `:83` (packages),
   `:96` (provides), `:99` (requires java.base). Fix: build the mirror from
   `ModuleRegistry` — the data is all there once patch 1 lands.
2. **`ModuleLayer.findModule` returns a fresh Module, not the canonical
   mirror.** `Class.getModule()` publishes one mirror per module name through
   `ctx.cache_module_mirror` (`native-builtins/src/lib.rs:11751`);
   `findModule` (`jboss_jdkspecific.rs:436`) ignores that cache. Trips
   `:129` / `:130` (`Greeter.class.getModule() == svc`) and `:165`.
   Fix: `get_cached_module_mirror(Some(name))` first.
3. **`findModule` has no negative answer.** Same site. Needs a real existence
   test against the registry; `NativeContext` has `module_packages` /
   `module_uses` / `module_is_open` but no `module_exists`, so a new trait
   method (default `true` for mocks, registry-backed in `vm_exec.rs`) is
   required. Until then check 1 is vacuous and `svc()`'s diagnostic message
   can never be seen — which is exactly why this bug presented three checks
   downstream of its own cause.
4. **A named module reads the unnamed module.** `ModuleRegistry::reads`
   (`classloading/src/module.rs`) returns `true` whenever
   `provider == UNNAMED_MODULE`. Trips `:114`
   (`!svc.canRead(unnamed)`). This is the deliberate classpath-compat escape
   hatch — every app on the class path depends on it — so it must become
   mode-gated, not removed.
5. **The unnamed module can access a non-exported package.**
   `ModuleRegistry::check_module_access` returns `Ok(())` immediately when
   `accessor_module == UNNAMED_MODULE`. JDK 17+ strong encapsulation does not
   work that way (`check_deep_reflection_access`, three functions below it,
   already gets this right and documents the difference). Trips `:172`
   (instantiating `internal.EnGreeter` from the class path must throw
   `IllegalAccessException`). Same mode-gating caveat as (4).
6. **`Module.getResourceAsStream` is unimplemented.** No registration for it
   anywhere in `native-builtins`. Needs the "`.class` files are never
   encapsulated, other resources follow `opens`" rule. Trips `:192`, `:198`,
   `:204`, `:208`.
7. **`ServiceLoader.Provider.type()` for a `provider()` static factory.**
   `RJdkModule.java:234` expects `["EnGreeter", "Greeter"]` — the JDK builds
   `ProviderImpl` with `factoryMethod.getReturnType()`, so the factory
   provider reports the *service* type, not `FactoryGreeter`. Module-path
   provider discovery itself should work: `ServiceLoader` already consults
   `ctx.service_providers_from_modules` (`native-builtins/src/service_loader.rs:1227`),
   which reads `ModuleRegistry::service_providers`.

`Module.isExported` / `isOpen` (`:119`-`:126`) are already registry-backed
(`native-builtins/src/lib.rs:11810-11875`) and should start answering correctly
the moment patch 1 registers the descriptor non-automatically — those six
checks are expected to pass with no further work.

## Verifying

The `--module-path` / `--add-modules` flags are **mandatory**; a verifier who
omits them reproduces the original harness mistake and learns nothing.

```
cd regression-suite

# oracle
"/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/java" \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc \
    -cp build RJdkModule
# expected: PASS RJdkModule (44 checks), rc=0

# both CratonVM modes
cratonvm.exe --real-jdk --module-path build-modules \
    --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
cratonvm.exe --jdk-only --module-path build-modules \
    --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
```

`regression-suite/run.sh:142` (`class_args`) already appends these two flags for
this vector and only this vector, so `./run.sh RJdkModule` is correct as-is.

Rust-side:

```
cargo test -p cratonvm-classloading --lib module::tests
```

## What would falsify the diagnosis

A run that reaches `RJdkModule.java:57` with `m.getLayer()` and
`ModuleLayer.boot()` already being the *same* object. That would mean some
other registration is answering `ModuleLayer.boot()` ahead of
`jboss_jdkspecific::native_module_layer_boot` — `native-builtins/src/phases_late.rs`
registers the same triple (`register_p59_module_layer`, reachable only through
the `synthetic-jdk`-gated `register_synthetic_overrides`) — and the identity
half of this report would be wrong, leaving only the (c) starvation.

The cheap way to check before touching anything: run the vector **without**
`--module-path`/`--add-modules`. If it still fails at line 57 rather than at
line 48, the flags are provably not participating, which is claim (c).
