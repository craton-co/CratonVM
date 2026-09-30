# Proposal: one module-access question, asked by identity, for resolution, reflection and lookups

**Status: proposal — filed 2026-10-09 by interpreter round i1 wave 45, lane
L5. Not implemented.**

## The problem

After waves 43-45 a class's module is known by identity
(`access_control::module_ident_of`, `NativeClassAccess::module_identity_of_class`),
but the three questions JVMS §5.4.4 and the JDK ask about a pair of modules --
does `a` READ `b`, does `b` EXPORT `p` to `a`, does `b` OPEN `p` to `a` --
are still answered in several places, each with its own copy of the
special cases:

* `classloading/src/access_control.rs` `class_export_denial` (a
  `CONSTANT_Class` resolution): name-map exports, `layer_module_export_verdict`,
  the layer accessor's `is_package_exported_to_layer_module`, and
  `layer_accessor_readability_denial`;
* `vm/src/vm/vm_exec.rs` `check_deep_reflection_access` and
  `reflective_export_to_accessor` (the reflective gates): the same three
  arms again, with `ModuleRegistry::check_deep_reflection_access`'s own
  readability test for name-map modules;
* `native-builtins/src/lang_class.rs` `enforce_set_accessible_gate`: the VM
  gate, then `layer_target_not_open` (the JDK's `implIsExportedOrOpen` for a
  layer target), then `set_accessible_export_carve_out`, then the
  `ClassLoader.defineClass` carve-out;
* `native-builtins/src/lang_invoke.rs` `pli_enforce` (`privateLookupIn`):
  no module question at all
  (`docs/internal/fixed-bugs/interpreter-L5-privatelookupin-admits-a-target-whose-package-is-not-open-FIXED-20261010.md`);
* the `Module.canRead` / `isExported` / `isOpen` natives
  (`jboss_jdkspecific.rs`, `lib.rs`): the name registry, or the JDK's body for
  a layer module (wave 43).

A layer module's `opens` exist only in the JDK's `Module` fields, so one of
the copies must call Java; the others cannot, and so disagree with it.

## The direction

1. **Record `opens` by identity.** The JDK tells the VM nothing about opens,
   but its own record is complete when `Module.defineModules` returns:
   `m.openPackages` (static) and `ReflectionData.exports` (run-time,
   `implAddOpens` -> `implAddExportsOrOpens(open = true)`, whose natives are
   ours for a layer module since wave 43). Read `openPackages` once per layer
   module when `ModuleLayer.defineModules*` returns (or lazily on the first
   question), and record every run-time `addOpens` in the native that already
   routes it; keep them in `ModuleRegistry`'s `LayerModule` next to the
   exports and reads it holds since waves 44-45.
2. **One question.** `access_control::module_access(accessor: ModuleIdent,
   target: ModuleIdent, pkg, Question::{Read, Export, Open})` answers from
   the name map or the layer record, by identity, with the name map's
   `ALL-UNNAMED` / qualified-edge rules in one place. `class_export_denial`
   asks `Read` then `Export`; the reflective gates `Export` / `Open`;
   `setAccessible` `Open` then the JDK's exported-and-public arm;
   `privateLookupIn` `Read` then `Open`; the `Module` natives the same
   questions for a `Module` object's identity. `layer_target_not_open` and
   the wave-43 Java deferrals then retire.
3. **Messages** from the same place: HotSpot's `TYPE_NOT_EXPORTED` /
   `MODULE_NOT_READABLE` texts and the JDK's `InaccessibleObjectException` /
   `IllegalAccessException` texts, each with `ModuleIdent::describe`.

## Probes and measure

`L5W44LayerModuleAccess`, `L5W45LayerModuleOpens`,
`L5W45LayerAccessorOfBootModule`, `L5W45PrivateLookupInOpens`,
`L5W43UserLayerModule`, `L5W28JdkNamedModule`, `RJdkModule` and
`ThreadGroupLayoutProbe` must keep (or reach) HotSpot's lines; the suite, the
jdk-only corpus, Spring Boot and JBoss Modules under `CRATONVM_DBG=access`
must show no new refusal. `--compatible` keeps its answers (a mode test in
the one function).

## Progress (wave 46) — lane L5: the reflective gates' duplicate retired

Built: the part of item 2 that retires a duplicate. `vm_exec.rs`
`check_deep_reflection_access` and `reflective_export_to_accessor` carried
two copies of the same three arms (a layer module's class as the target, a
layer class as the accessor of a name-map module, the name map); both are
now one call to `classloading/src/access_control.rs`
`reflective_module_access(accessor, target, registry,
ReflectiveModuleQuestion::{Export, Open})`, which holds the arms once. No
behaviour change for those arms (unit test
`the_reflective_module_question_is_asked_by_identity`). The same function
gained, in a separate commit, the one new rule of wave 46: an unnamed
accessor is admitted when the package was opened (or exported) to its own
loader's unnamed module by identity
(`module::unnamed_module_of_loader_target`, recorded by the
`implAddOpens` / `addOpens` natives under `--jdk-only`; the JaCoCo
`Instrumentation.redefineModule` + `privateLookupIn` fix, probe
`tools/probes/interp/L5/L5W46RedefineModuleOpensToAgent.java`).

Not built (what remains of the direction):

* **Item 1, opens by identity for a layer module**, and with it the
  retirement of `lang_class.rs` `layer_target_not_open` (the JDK call the
  `setAccessible` gate makes after the VM's export-only answer) and of the
  wave-43 `Module` native deferrals.
* **`class_export_denial`** (the `CONSTANT_Class` question: readability,
  then exports) still asks its own arms; folding it into the one function
  needs a `Read` question and the `TYPE_NOT_EXPORTED` /
  `MODULE_NOT_READABLE` messages there (item 3).
* **`privateLookupIn`** (wave 45's `classloader::private_lookup_in_module_refusal`)
  asks the JDK's `Module.canRead` / `isOpen` through the natives, so it
  agrees with them by construction; moving it onto the one function is
  item 2's last caller.
