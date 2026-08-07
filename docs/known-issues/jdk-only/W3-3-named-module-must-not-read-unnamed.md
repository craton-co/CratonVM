# A named module implicitly read the unnamed module, because every named module looked unnamed

Status: fix written, unbuilt (lane W3-3 cannot run cargo or the VM).
Applies to: **both** `--real-jdk` (Compatible) and `--jdk-only` (JdkOnly),
identically. HotSpot 25 passes all 44 checks with exit 0, so this is an
ordinary Compatible-mode defect, not a strict-mode policy question.

## The failure

`regression-suite/src/RJdkModule.java`, both arms, byte-identical output:

```
CK RJdkModule exports=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.open] opens=[...] packages=[...] provides=[com.cratonvm.jdkonly.svc.Greeter->2]
Exception in thread "main" java/lang/AssertionError: a named module must NOT implicitly read the unnamed module
    at RJdkModule.main(RJdkModule.java:248)
    at RJdkModule.readabilityAndExports(RJdkModule.java:114)
```

```java
// RJdkModule.java:113-116
check(unnamed.canRead(svc), "the unnamed module must read every resolved module");
check(!svc.canRead(unnamed),
        "a named module must NOT implicitly read the unnamed module");
check(svc.canRead(Object.class.getModule()), "svc reads java.base");
```

Run with the module flags — omitting them changes what is being measured:

```
cd regression-suite && <cratonvm> --java-home "<jdk>" [--jdk-only] \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
```

## The oracle

Measured on HotSpot 25 (`jdk-25.0.3.9-hotspot`), not asserted from the spec:

```
unnamed.isNamed=false name=null
unnamed.canRead(java.base)=true
unnamed.canRead(java.logging)=true
java.base.canRead(unnamed)=false
java.logging.canRead(unnamed)=false
java.logging.canRead(java.base)=true
java.base.canRead(java.logging)=false
java.base.canRead(java.base)=true
# and with --add-reads java.logging=ALL-UNNAMED:
java.logging.canRead(unnamed)=true
```

The unnamed-module readability rule is **directional**: the unnamed module
reads every module; no named module reads the unnamed module unless an
explicit `addReads` / `--add-reads m=ALL-UNNAMED` grant says so.

## Two independent defects, both required

`Module.canRead` is registered in `register_essential_natives`
(`native-builtins/src/lib.rs`) and **already wins over the real bytecode** —
`resolve_step1_native` (`vm/src/runtime/interpreter/native_override.rs`)
resolves the triple in the registry and dispatches whatever it finds, with no
list, before `force_native_over_real_jdk_bytecode` runs at all. So the wrong
answer comes from the native, not from unpopulated real-JDK fields. (That the
strict arm fails at the *same* line corroborates it: under `--jdk-only` the
shadow observation is unenforced by default, so `bytecode_available` is `false`
and the bridge keeps the call there too.)

### 1. `read_module_name` read slot 0, which is `layer`

`native-builtins/src/phases_late/reflect_invoke.rs::read_module_name` did

```rust
match ctx.get_field(module_obj, 0) { ... }
```

`javap -p java.lang.Module` (JDK 25) declares `layer` first and `name` second:

```
private final java.lang.ModuleLayer layer;   // slot 0
private final java.lang.String name;         // slot 1
private final java.lang.ClassLoader loader;
private final java.lang.module.ModuleDescriptor descriptor;
```

`read_string` refuses any object whose class is known and is not
`java/lang/String`, so the read answered `None` → `""` — the unnamed-module
sentinel. **Every** real-JDK `Module` therefore looked unnamed to every native
routed through this helper, including `native_module_can_read`.

Two sibling helpers already had the right shape and were not shared:
`jboss_jdkspecific::module_registry_name` and lib.rs's local
`module_name_of_mirror`, both "declared `name` field first, slot 0 as
fallback". `read_module_name` is the third copy and was the one that never got
the fix; it now matches. (The slot-0 fallback is still needed: the synthetic
`java/lang/Module` built by `register_p59_module` parks the name at slot 0, and
the fabricated fallback layout in `class_manager.rs` names its fields `_f0..`,
so `get_field_by_name(_, "name")` finds nothing there.)

### 2. `ModuleRegistry::reads` was symmetric where JPMS is directional

`classloading/src/module.rs::reads` returned `true` for
`provider == UNNAMED_MODULE` as well as for `reader == UNNAMED_MODULE`.

With defect 1 in place the reader was `""` and the first arm fired; with defect
1 fixed the second arm fires. **Fixing either alone leaves `:114` failing** —
which is why this reads as "no progress" if attempted piecemeal.

## The fix, and why the classpath escape hatch survives

The `reader == UNNAMED_MODULE` arm is kept — that is the escape hatch, and it
is the direction JPMS actually grants. Only the `provider == UNNAMED_MODULE`
arm is removed. It is not mode-gated, because nothing that depends on the
escape hatch goes through it:

* `check_module_access` returns `Ok(())` for `accessor_module == UNNAMED_MODULE
  || target_module == UNNAMED_MODULE` **before** it calls `reads`.
* `check_deep_reflection_access` returns `Ok(())` for `target_module ==
  UNNAMED_MODULE` **before** it calls `reads`.
* A classpath jar carrying a `module-info.class` is registered `automatic`, and
  the automatic arm returns `true` for every provider — `org.jboss.logging`
  and friends are untouched.
* A reader with no registered descriptor keeps the open-world `true`.
* Explicit grants still work through `extra_reads` on both the built-graph and
  un-built-fallback paths: `Module.addReads(unnamed)`, the `Module.addReads0`
  VM-sync hook (Mockito's `InlineBytecodeGenerator.assureCanReadMockito` makes
  `java.base` read the unnamed Mockito module exactly this way), and
  `--add-reads m=ALL-UNNAMED`.

That leaves only the JPMS query surface — `NativeContext::reads_module`, i.e.
`java.lang.Module.canRead` — reaching the deleted arm, which is precisely where
the directional answer is the correct one.

## Companion patch outside this lane's files

`vm/src/config.rs::parse_add_reads` does **not** translate the `ALL-UNNAMED`
sentinel to the empty string, unlike its sibling `parse_add_exports` which
documents doing exactly that. Today `--add-reads m=ALL-UNNAMED` "works" only by
accident, via the blanket rule this change removes; without the companion patch
the flag silently stops granting anything. See the lane report for the exact
patch.

## Still open on this vector (not fixed here)

`RJdkModule` asserts 44 checks and reached ~14. Behind `:114`:

* `Module.getResourceAsStream` (`:190-:208`) — registered by wave 2 in
  `jboss_jdkspecific`, force-listed, but its encapsulation answers are
  unverified.
* `moduleServices()` (`:217-:242`) — needs module-path `provides` to feed
  `ServiceLoader`, including the `provider()` static-factory form, whose
  `Provider.type()` is the factory method's **return** type.
