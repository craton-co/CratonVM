# Proposal: a class's module is one identity every reader sees, not a name each reader reinterprets

**Status: proposal, stage 1 built — filed 2026-10-08 by interpreter round
i1 wave 44, lane L5. Wave 45 (lane L5) built the one question natives ask
(`module_identity_of_class`) and moved two readers onto it; see "Progress
(wave 45)".**

## Progress (wave 45) — lane L5: one identity question for the natives

Built (`--jdk-only` answers change for layer classes only; every other
class gets `module_name_of_class`'s answer, so `--compatible` is unchanged):

* `NativeClassAccess::module_identity_of_class(class_id) ->
  ClassModuleIdentity::{Named(name), Layer { loader_ns, name, version },
  Unnamed}` (`native-api/src/registry.rs`), answered in `vm_exec.rs` from
  `access_control::layer_module_of` (now `pub`), the same function the VM's
  access control and `Class.getModule()`'s record agree with. The default
  (every other `NativeContext`) is `module_name_of_class`'s answer.
  `ClassModuleIdentity::describe` is HotSpot's `module m` / `unnamed module`.
* `Module.defineModule0` records a layer module's version
  (`ModuleRegistry::set_layer_module_version`), and `addReads0` its reads
  (`add_layer_read`, used by the new readability clause; the i42 page).
* Readers moved: a stack trace element's `moduleName` / `moduleVersion`
  (`lang_misc.rs` `frame_module`: HotSpot prints `m45e@2.1/p45e.Hello.boom`)
  and the duplicate-definition message (`classloader_real.rs`
  `duplicate_definition_message_for`), and the message of the new
  `setAccessible` opens refusal. Probe
  `tools/probes/interp/L5/L5W45LayerModuleOpens.java`, rows `ste` and
  `duplicate`.

Not built -- the readers still on `module_name_of_class`, each with what it
needs (a layer class reads as unnamed in all of them):

* `jboss_jdkspecific::resource_caller_module` (`Module.getResourceAsStream`'s
  encapsulation test by caller module): needs the encapsulation check to take
  a `ClassModuleIdentity` and ask a layer target's `isOpen` as the
  `setAccessible` gate now does.
* `reflect_annotations.rs` `proxy_interfaces_non_exported` (and the proxy
  module helpers near lines 4416 / 4533 / 4562): a layer interface's
  unqualified-export question (`!m.isExported(pn)`) needs a registry query
  for a layer module -- open, or its export list for the package holds
  `LayerExportTarget::Everyone`; today a layer interface reads as an
  unnamed-module one, so a package its module does not export counts as
  exported.
* `lang_invoke.rs` `lk_module_key` (`Lookup.in` module moves): a layer class
  is keyed as its loader's unnamed module, so a move between two layer
  modules of one loader is not seen as a module change.
* `deprecated_internal.rs` `native_reflection_ensure_native_access`
  (`--enable-native-access` by module name) and `lang_class.rs` 6910 (the
  `ClassLoader.defineClass` carve-out): a layer module's name is what
  `--enable-native-access=m` names on HotSpot.
* Directions 1-2 (a `ModuleKey` on `Class` decided at define, one per-VM
  table holding the `Module` object) are unchanged: the identity is still
  derived at the question from the loader's package map, which is cheap (one
  `is_empty` test in a VM with no layer module) and cannot disagree with the
  record `Class.getModule()` reads.


## Where wave 44 stopped

A class of a user `ModuleLayer`'s module is now judged by its module's
identity -- `(defining loader namespace, module name)`, from the loader's
package map (`access_control::layer_module_of`) -- in JVMS §5.4.4's export
clause, the reflective gates and two messages. But the class's own
`Class::module_name` is still the NAME map's answer (`None` for a layer
class), and at least four readers each re-derive the module their own way:

* `Class.getModule()` (`native-builtins/src/lib.rs`): the wave-43
  `(loader, package) -> Module` record, then `module_name`, then the proxy
  module, then the loader's unnamed module;
* access control: `module_ident_of` (layer map, then `named_module_of`,
  which drops class-path-only and a user loader's platform-named module);
* `NativeContext::module_name_of_class` (about fifteen natives: resource
  lookup, `StackTraceElement`, `Package`, proxies, annotations), which
  answers `module_name` and so calls a layer class unnamed;
* the messages (`exceptions.rs` `message_module_of`, `access_control`
  `class_in_module_of_loader`), which read `module_name` or the layer map.

They agree today only because each carries the same special cases, and a
fifth reader will miss one (wave 44's review found the reflective gates and
the `ClassCastException` origin still on `module_name`).

## The direction

1. `Class` gets one `module: ModuleKey` (a small interned id into a per-VM
   module table: named-map module, layer module, a loader's unnamed module),
   decided ONCE at define by one function -- the rules now spread over
   `define_class_shared_with_options` ("Determine this class's module
   membership"), `member_module_of`, `named_module_of`, `layer_module_of` and
   the proxy module -- and `module_name` becomes a view of it.
2. The per-VM module table holds, per key, the name, the descriptor (or the
   layer module's exports / opens / reads, filled by the `Module` natives by
   identity as wave 44 does for exports), and the `java.lang.Module` object
   once one exists (the mirror cache and the wave-43 record fold into it).
3. Every reader above asks the key. `module_name_of_class` answers the key's
   name, so a layer class's natives see its module; `Class.getModule()`
   answers the key's object.

## Probes and measure

`L5W43UserLayerModule`, `L5W44LayerModuleAccess`,
`L5W28JdkNamedModule`, `L5W26SerializationAccessorSpoof`, the Tomcat
`catalina.jar` class-path-modular-jar case (`named_module_for_package`'s
note), the proxy-module rows, and `regression-suite/src/RJdkModule.java`
must keep their lines; the suite, the jdk-only corpus, Spring Boot and JBoss
Modules under `CRATONVM_DBG=access` must show no new `IllegalAccessError`.
`--compatible` keeps its labels (the owner's call), which the one define-time
function can express as a mode test in one place.
