# Proposal: a user layer's class is a member of its module when it is defined, and the module registry knows layer modules by identity

**Status: proposal, partly built — filed 2026-10-07 by interpreter round i1
wave 43, lane L5. Wave 44 (lane L5) built items 1 and 2, with membership
decided from the loader's package map when access is asked rather than
stored at define; item 3 is open. See "Progress (wave 44)".**

## Progress (wave 45) — lane L5

Item 3 (retire the wave-43 deferrals of `canRead` / `isExported` / `isOpen`
to the JDK's bodies) is still not built, but one of its inputs now exists:
the VM records a layer module's READS by identity (`Module.addReads0` ->
`ModuleRegistry::add_layer_read`), which the readability clause of a layer
class's `CONSTANT_Class` resolution reads
(`i42-L5-a-class-of-a-named-module-in-a-user-layer-…`, Progress (wave 45)).
`opens` are still the JDK's alone (the `setAccessible` gate now asks the
JDK's `implIsExportedOrOpen` for a layer target), so retiring the `isOpen`
deferral would first need an opens record the JDK never sends to the VM;
keep it.

## Progress (wave 44) — lane L5

**Built (`--jdk-only`; `--compatible` records nothing):**

1. **A registry entry per layer module, keyed by identity.**
   `classloading/src/module.rs`: `ModuleRegistry::define_layer_module(loader_ns,
   name, is_open, packages)` keeps `(loader namespace, module name) ->
   LayerModule { is_open, exports }` apart from the name map, and
   `layer_packages` (loader namespace -> package -> module name) as HotSpot's
   per-loader package-to-module map. `add_layer_export` records an export by
   identity: `LayerExportTarget::{Everyone, AllUnnamed, Named, Layer,
   Unnamed { loader_ns }}`. The natives fill it:
   `jboss_jdkspecific::native_module_define_module0` (a module whose `layer` is
   set and whose loader is user-defined; a layer-less dynamic module such as a
   proxy's `jdk.proxyN` is left out) through the new
   `NativeClassAccess::layer_module_define`, and `addExports0` /
   `addExportsToAll0` / `addExportsToAllUnnamed0` of such a module
   (`layer_module_add_export`, identity of `from` and `to` read from the
   `Module` objects) through `layer_module_add_export` -- no longer by name.
   An open layer module's `defineModule0` no longer writes name-keyed opens.
2. **Membership.** Not a field written at define: `module_name` stays the
   name map's answer, and `access_control::layer_module_of(class, registry)`
   answers the layer module from the defining loader's package map when an
   access question is asked (one `is_empty` test in a VM without layer
   modules). That is HotSpot's own rule (a class is in the module its
   loader's `PackageEntry` names), needs no change to `Class` or to the
   define path, and cannot disagree with `Class.getModule()` (the wave-43
   record, written by the same `defineModule0`). `module_ident_of` gives
   either side of a check a `ModuleIdent::{Named, Layer, Unnamed}`.
   `class_export_denial`, `vm_exec.rs` `reflective_export_to_accessor` /
   `check_deep_reflection_access`, the `ClassCastException` origin
   (`exceptions.rs` `message_module_of`) and the loader-constraint origin
   (`ClassManager::loader_constraint_class_origin`) read it.

Probe `tools/probes/interp/L5/L5W44LayerModuleAccess.java` (the rows this
proposal asked for: a non-exported package refused at a `CONSTANT_Class`
resolution and reflectively, two layers each defining `m44c` with different
exports; plus qualified, controller, open and cast rows). HotSpot's output
and the base's are in its header; the positive control is
`CRATONVM_DBG=access`'s `[LAYER-MODULE] addExports ...` lines.

**Not built:**

* Item 3, retiring the wave-43 deferrals (`jboss_jdkspecific::
  is_user_layer_module` routing `canRead` / `isExported` / `isOpen` / the
  `implAdd*` natives to the JDK's bodies). They answer correctly today, and
  the VM record holds no `opens` or `reads` to answer them from, so retiring
  them would first need those (the JDK's `Module` fields are the source).
* `opens` for the deep-reflection gate, and a layer class as the accessor of
  a name-map module: on the i42 page's "What remains".

**Measure:** as the proposal says -- the suite, the jdk-only corpus, JBoss
Modules and the Spring Boot fat jar under `CRATONVM_DBG=access` must show no
new `IllegalAccessError` (none of them defines a `ModuleLayer`, so nothing is
recorded for them).


## Where wave 43 stopped

`i42-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module`
now has, under `--jdk-only`, a per-VM record `(loader namespace, package) →
Module` written by `Module.defineModule0` (`ClassRealm::layer_module_packages`)
and read by `Class.getModule()`; a layer module's `canRead` / `isExported` /
`isOpen` run the JDK's own bodies. What the VM itself believes about the class
did not change: `Class::module_name` is decided at define
(`classloading/src/class_manager.rs`, "Determine this class's module
membership from the package registry") from the NAME-keyed `ModuleRegistry`,
which has no layer module, so it stays `None`. Every VM-side check that reads
`module_name` sees an unnamed-module class:

* `classloading/src/access_control.rs` `check_module_access` /
  `class_export_denial` (JVMS §5.4.4's export clause at resolution): a class of
  another module can name a layer module's NON-exported package, where HotSpot
  throws `IllegalAccessError` ("module m does not export p to …");
* the reflective access checks that ask the registry by module name
  (`NativeContext::check_deep_reflection_access`, `is_package_open_to`);
* every other native that asks `NativeContext::module_name_of_class` for the
  class (only `Class.getModule()` consults the per-loader record).

## The direction

1. **Membership at define.** When a user-defined loader defines a class, the
   define path asks the per-loader record for `(loader, package)` before the
   name registry (the record is written before any class of the module can
   be defined: `Module.defineModules` runs `defineModule0` for every module
   before it returns the layer). The class gets a module KEY, not a name: two
   layers may each define a module `m` (the plugin arrangement layers exist
   for), so a name cannot identify it.
2. **A registry entry per layer module, keyed by identity.** `ModuleRegistry`
   gets entries for layer modules under a VM-unique key (the record's
   `(loader namespace, module name)` is unique: a loader cannot hold two
   modules of one name), filled from the JDK's own `Module` fields at
   `defineModule0` (packages) and kept in step by the export / open / read
   natives the JDK already calls (`addExports0`, `addExportsToAll0`,
   `addExportsToAllUnnamed0`, `addReads0`), which today record by module name.
   `access_control.rs` then answers a layer module's exports as it answers a
   platform module's.
3. **Retire the name-keyed natives for layer modules.** With 1 and 2, the
   deferrals wave 43 added (`jboss_jdkspecific::is_user_layer_module`) can go:
   the natives answer from the identity-keyed entry.

## Probes

`tools/probes/interp/L5/L5W43UserLayerModule.java` must keep HotSpot's lines.
New rows, HotSpot-measured before building: a class of the probe's unnamed
module naming `q.Other` of a non-exported package of a layer module (a
`CONSTANT_Class` resolution: `IllegalAccessError`, and the reflective
`Class.forName(...).getMethod(...).invoke` → `IllegalAccessException`); two
layers each defining a module `m` with different exports.

## Measure

JBoss Modules and the Spring Boot fat jar define classes through their own
loaders without JPMS layers and must keep their answers; Elasticsearch-style
plugin layers (`ModuleLayer.defineModulesWithOneLoader`) are the workload that
changes. The suite and the jdk-only corpus under `CRATONVM_DBG=access` must
show no new `IllegalAccessError`.
