# Dynamic and annotation proxies: identity facts that still differ from HotSpot

Status: OPEN
Area: `native-builtins/src/lib.rs` (`Class.getModule`), `native-builtins/src/reflect_annotations.rs` (`proxy_check_module_access`), `classloading/src/class_manager.rs` (the `AnnotationProxy` carrier's interfaces), `vm/src/vm/vm_exec.rs` (`annotation_proxy_dispatch_impl`'s `getClass` arm), `native-builtins/src/lang_class.rs` (`ctx_format_annotation_value`, `create_annotation_proxy_with_type`)
Severity: LOW-MEDIUM (wrong answers from reflection on proxies; no known application failure)
Found by: round 13 wave 8 lane proxy5 (carried from `r13w4-proxy2-proxy-dispatch-residuals-FIXED-20260928.md` item 3 and `r13w6-proxy4-annotation-proxy-conformance-FIXED-20260928.md` "Left")

Measurement probe: `C:\craton\jitr13-probes\src\R13Proxy5Residuals.java` (hot-loop counts, HotSpot
expected `miss 0`). Each item names the line that shows it.

## 1. An all-public proxy is in the unnamed module (`module`)

Since JDK 16 `ProxyBuilder.mapToModule` puts every all-public proxy in its loader's dynamic module
`jdk.proxyN`: `p.getClass().getModule().isNamed()` is `true`, `getName()` is `jdk.proxy1`. This VM
names the package `jdk/proxyN` (`build_proxy_spec_for`) but `Class.getModule`
(`native-builtins/src/lib.rs` ~14087) asks `module_name_of_class`, which knows no such module, and
answers the loader's unnamed module. Fix: at define time register a per-(VM, loader) module
`jdk.proxyN` (open, reads every module it needs, like `ProxyBuilder.getDynamicModule`) in the module
registry and record the proxy class in it; `proxy_module_number` already gives N. The same module is
what `proxy_check_module_access`'s named-TARGET case needs (today it decides only for an unnamed
target, `r13w4` item 3); do both together.

## 2. `Proxy.getInvocationHandler(annotation)` is not an `InvocationHandler` (`handler`)

HotSpot returns a `sun.reflect.annotation.AnnotationInvocationHandler`. This VM returns its carrier
`java/lang/annotation/AnnotationProxy`, whose only declared interface is `Annotation`
(`class_manager.rs` ~13977), so `h instanceof InvocationHandler` is `false` (calls still work: every
call on the carrier is routed by name, `invoke` included). Worse, `h.getClass()` answers the
ANNOTATION interface (`annotation_proxy_dispatch_impl`'s `"getClass"` arm, `vm_exec.rs`, a
Spring-era stop-gap for the bare-carrier representation that `CRATONVM_REAL_ANNOTATIONS=0` still
selects). Fix, in order: add `java/lang/reflect/InvocationHandler` to the carrier's interface list
(`class_manager.rs`), then answer `getClass` on a carrier with the carrier's own class when real
annotations are on (the annotation-typed answer only for the bare-carrier representation). Both need
a Spring battery run (the arm was added for `MergedAnnotation.adaptForAttribute`).

## 3. A `Class`-valued member naming a nested class in `toString` (`classValue`)

`ctx_format_annotation_value` renders a `Class` member with `getCanonicalName()`
(`java.util.Map.Entry.class`). This lane's reading of `AnnotationInvocationHandler.toSourceString(Class)`
is `getName()` of the final component plus `[]`s (`java.util.Map$Entry.class`), while the ANNOTATION
TYPE itself is rendered canonically (measured, `ctx_annotation_type_canonical_name`). Not changed
blind: the probe line settles it; if HotSpot prints `$`, render the member with the binary name
(both twins, `jdk_member_value_to_string` and `format_annotation_value`).

## 4. Spring `@Reflective`: the builder rewrites one alias member into the other

`create_annotation_proxy_with_type` (`lang_class.rs`, the
`"Lorg/springframework/aot/hint/annotation/Reflective;"` block) copies an explicit `value` into
`processors` (and back) before the carrier is built, so `reflective.processors()` answers the
explicit classes where HotSpot answers the declared default `{SimpleReflectiveProcessor.class}`, and
`equals` / `hashCode` / `toString` of that annotation differ from HotSpot's. It is a class-name
allow-list in the annotation builder (AGENTS.md) and a workaround for a different defect, recorded in
its own comment: the default `Class` value resolved "loader-distinct" after a forked test populated
Spring's caches. Fix: find why the `AnnotationDefault` `Class` resolves to another loader's copy
(`annotation_element_to_java_typed` with `default_owner`), then delete the block. Confirm with Spring's
`@CompileWithForkedClassLoader` AOT tests on the Linux host, before and after.

## Confirm (wave 8; superseded by `R13Proxy6Residuals`, see the wave 9 section)

`R13Proxy5Residuals`: `miss 0` and HotSpot's `module` / `handler` / `classValue` lines in every arm.

## Round 13 wave 9 (lane proxy6)

**The `R13Proxy5Residuals` difference is real, but the probe cannot measure it.** Its `miss` line
summed three independent facts. Two are deterministic and real (30 000 each: the proxy's module
is unnamed, the handler is not an `InvocationHandler`). The third, `h.getClass() == R.class`, is
tier-dependent: the interpreter's carrier intercept answers the annotation type, compiled code
answers the carrier class from the header, so it counted only the ~550-590 iterations run before
`hot` was compiled -- 60545..60590 across the w8b/w8d arms, a different number every run. Its
`handler` line printed the handler class NAME, which cannot match HotSpot's
`sun.reflect.annotation.AnnotationInvocationHandler`, and flipped to `R13Proxy5Residuals$R` under
`CRATONVM_JIT_OSR=0` (it measured whether `main` was OSR-compiled). Replacement probe:
`C:\craton\jitr13-probes\src\R13Proxy6Residuals.java`, one counter per fact, each 0 on HotSpot by
construction, no HotSpot-chosen class name printed.

**Item 1 (`module`) landed** (both modes, `CRATONVM_PROXY_DYNAMIC_MODULE`, default on):
`Class.getModule()` (`native-builtins/src/lib.rs`) answers a named `jdk.proxyN` for a generated
`jdk/proxy<N>/$Proxy<M>` class (`reflect_annotations.rs::proxy_dynamic_module_of`), one canonical
Module per name, with `loader` = the proxy's loader, its package exported unconditionally and open
to `java.base` only (dynamic registry edges, as `getDynamicModule`). Presentation only: the class's
`module_name` stays `None`, so linkage and access checks still treat it as unnamed (lenient), and
the module is not in the registry. Left: `getLayer()` answers the boot layer (HotSpot `null`:
`native_module_get_layer`'s named path), `getDescriptor()` is the empty shape (HotSpot: `SYNTHETIC`,
packages `{jdk.proxyN}`), `getPackages()` is empty; and a proxy of a public interface in a
NON-exported package is still named `jdk/proxyN/...` where HotSpot uses a non-exported
`com.sun.proxy.jdk.proxyN` package. None is on a known application path.

**Item 2 (`handler`)**: the carrier now implements `java/lang/reflect/InvocationHandler`
(`ClassManager::fabricate_class`, switch `CRATONVM_ANNOTATION_CARRIER_IS_HANDLER`, default on), so
`instanceof` / checkcast agree with HotSpot (dispatch is unchanged: calls on a carrier are routed by
name before any lookup, and CHA binds behind a receiver guard). The `getClass` arm is
`vm_exec.rs`, not this lane's file: exact patch in
`r13w9-proxy6-vm-exec-annotation-carrier-getclass-patch-FIXED-20260928.md` (same switch). The class NAME
stays `java.lang.annotation.AnnotationProxy`, and `h.toString()` / `hashCode()` / `equals()` on the
handler still answer the annotation's (HotSpot: `Object`'s) -- the carrier is shared with the bare
representation; not on a known path.

**Item 3 (`classValue`) is not a defect.** The HotSpot reference (`ref13/R13Proxy5Residuals.txt`)
prints `@R13Proxy5Residuals.R(e=A, k=java.util.Map.Entry.class)`, byte-identical to CratonVM in
every arm: a nested `Class` member renders canonically and an enum constant by `name()`. No change.

**Item 4 (`@Reflective`)**: unchanged; `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY=0` (new, default
on = today's behaviour) skips the alias copy so the Spring AOT battery can be run with and without
it on the Linux host before the block is deleted.

Status stays OPEN: the `getClass` patch, the item-1 leftovers above, item 4.

## Round 13 wave 10 (lane misc10)

Item 1 leftovers:

* **`getDescriptor()` / `getPackages()` -- landed** (switch `CRATONVM_PROXY_MODULE_DESCRIPTOR`,
  default on, both modes). `Class.getModule()` (`native-builtins/src/lib.rs`) stores, for the
  `jdk.proxyN` Module, the descriptor `getDynamicModule` builds, built by the JDK's own
  `ModuleDescriptor.Builder` (`reflect_annotations.rs::proxy_dynamic_module_descriptor`):
  modifiers `{SYNTHETIC}`, packages `{com.sun.proxy.jdk.proxyN, jdk.proxyN}`, one unqualified
  export of `jdk.proxyN`, `requires mandated java.base`. The real `Module.getPackages()` answers
  `descriptor.packages()` for a named module, so it follows. Any failure (synthetic JDK) keeps the
  old registry-built shape.
* **Non-exported package -- landed** (switch `CRATONVM_PROXY_NONEXPORTED_PACKAGE`, default on):
  `build_proxy_spec_for` names a proxy whose interfaces are all public but at least one sits in a
  package its named module does not export unconditionally `com/sun/proxy/jdk/proxyN/$ProxyM`
  (`ProxyBuilder.proxyClassContext`'s `nonExported`); `proxy_dynamic_module_name` maps that shape
  to module `jdk.proxyN`. `getModule` now exports `jdk/proxyN` unconditionally and opens BOTH
  packages to `java.base` only, whichever the class is in (was: export and open the class's own
  package). Both names are bootstrap-prefix names for `is_bootstrap_class_name`, so no loader gate
  changes; linkage and access still treat the class as unnamed.
* **`getLayer()`**: not this lane's file; exact, conditional patch in
  `r13w10-misc10-jboss-module-getlayer-patch-FIXED-20260928.md`.
* Probe: `C:\craton\jitr13-probes\src\R13Misc10ProxyModule.java` (every `miss.*` 0 on HotSpot).
* Unit test: `proxy_dynamic_module_name_reads_only_the_generated_shape` covers the new shape.

Status stays OPEN: the `getClass` patch (`r13w9-proxy6-vm-exec-annotation-carrier-getclass-patch-FIXED-20260928.md`),
the `getLayer` patch, item 4.

Wave 10 hand-back: `getLayer()` and `getPackages()` of the `jdk.proxyN` Module fixed in
`jboss_jdkspecific.rs` (both Bridges dispatch for it; see
`r13w10-misc10-jboss-module-getlayer-patch-FIXED-20260928.md`). Left: the `getClass` patch, item 4.

## Round 13 wave 11 (lane misc11)

Re-read at the current tree:

* **The `getClass` patch is applied**: `vm_exec.rs::annotation_proxy_dispatch_impl`'s `"getClass"`
  arm answers the carrier's own class when
  `lang_class::annotation_carrier_get_class_is_own()` (same switch,
  `CRATONVM_ANNOTATION_CARRIER_IS_HANDLER`), and the patch page is retired
  (`docs/internal/fixed-bugs/r13w9-proxy6-vm-exec-annotation-carrier-getclass-patch-FIXED-20260928.md`).
  Items 1-3 are therefore done.
* **The Module-mirror layer collision** that the wave-10 proxy fix worked around for `jdk.proxyN`
  only is now fixed for every module (`r13w10-misc10-module-mirror-name-written-into-the-layer-slot`,
  wave 11): the proxy case keeps its answer (slot 0 is not written on the real layout either way).
* **Item 4 (`@Reflective` alias copy) is unchanged and cannot be closed from here.** The block
  (`lang_class.rs`, `create_annotation_proxy_with_type`, behind
  `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY`) is a workaround whose trigger is a Spring AOT
  forked-loader run; reading the default path gives no reproducer: a default `Class` member is
  resolved with `default_owner = the annotation interface` and
  `classloader::defining_loader_for(vm, owner)` as the container loader
  (`annotation_element_to_java_typed`), which is `None` for an application-loaded `@Reflective`,
  so the `Class` arm falls back to `class_id_by_name_near(name, owner)` -- the annotation's own
  namespace, the one `Method.getDefaultValue()` uses. Whether the fork re-defines
  `org.springframework.aot.hint.annotation.*` (then `owner` is the fork's copy and
  `defining_loader_for` names the fork) is the question only the Linux Spring battery answers.
  Next step unchanged: run the Spring `@CompileWithForkedClassLoader` AOT tests on the Linux host
  with `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY=0` vs default; if identical, delete the block
  and its switch.
* Cosmetic handler residuals from wave 9 (the carrier's class NAME is
  `java.lang.annotation.AnnotationProxy`, and `h.toString()` / `hashCode()` / `equals()` on the
  handler answer the annotation's) are unchanged and remain off every known path.

Status stays OPEN (item 4 only).

## Round 14 wave 2 (lane trace)

Re-read at `adb9178bc`: the `@Reflective` alias-copy block (`lang_class.rs`
`create_annotation_proxy_with_type`, behind `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY`) is
unchanged and is still the only open item; nothing new can be learned without the Linux Spring
`@CompileWithForkedClassLoader` AOT run (default vs `=0`) that wave 11 named. No code change (and
`lang_class.rs` carries another lane's edits this wave). Status stays OPEN (item 4, pending the
host run; if identical, delete the block and its switch).

## Round 14 wave 3 (lane compat3)

Re-read at `20a1dbb4f`; no code change. Item 4 (`@Reflective` alias copy) is the only open item.
The block is `native-builtins/src/lang_class.rs` `create_annotation_proxy_with_type` (the
`"Lorg/springframework/aot/hint/annotation/Reflective;"` arm, behind
`CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY`), outside this lane's proxy regions of
`reflect*.rs` / `lang_reflect*.rs`. The proxy regions this lane owns were re-read for the
item-1/2 leftovers: `reflect_annotations.rs::proxy_dynamic_module_of` /
`proxy_dynamic_module_descriptor` / `proxy_check_module_access` match the wave 9-10 description;
nothing new found.

What would close item 4 is still one Linux-host run, which a lane cannot do: the Spring
`@CompileWithForkedClassLoader` AOT tests (the `/data/cvm/apps` Spring drivers) with
`CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY=0` against the default. Identical results: delete the
block and the switch (and its `jdk-only` kind-map/flag rows). A difference: the forked loader
re-defines `org.springframework.aot.hint.annotation.*` and the default `Class` member resolves in
the wrong namespace; then the fix is `annotation_element_to_java_typed`'s `default_owner` loader,
not the alias copy. Status stays OPEN (item 4, host run).

## Round 14 wave 4 (lane compat4)

Re-read; no code change. Item 4 (`@Reflective` alias copy, `native-builtins/src/lang_class.rs`
`create_annotation_proxy_with_type` ~17829, `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY`) is still
the only open item, and still sits outside this lane's proxy regions of `reflect*.rs`. The proxy
regions (`reflect_annotations.rs` `proxy_dynamic_module_of` / `_descriptor` /
`proxy_check_module_access`) are unchanged since wave 10 and match the page. What closes item 4 is
unchanged: the Linux Spring `@CompileWithForkedClassLoader` AOT run with the switch at `0` against
the default (identical: delete the block, the switch and its kind-map / flag rows; different: fix
`annotation_element_to_java_typed`'s `default_owner` loader instead). Status stays OPEN (item 4,
host run).

## Round 14 wave 6 (lane compat6)

Re-read at `74355aa4e`; no code change. Item 4 (`@Reflective` alias copy) is still the only open
item and still sits outside this lane's files: `native-builtins/src/lang_class.rs`
`create_annotation_proxy_with_type` ~17829 (the `"Lorg/springframework/aot/hint/annotation/Reflective;"`
arm behind `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY`), unchanged since wave 9. The proxy regions this
lane owns (`reflect_annotations.rs` `proxy_module_number`, `proxy_dynamic_module_of` / `_name`,
`proxy_interfaces_non_exported`, `proxy_dynamic_module_descriptor`, `proxy_check_module_access`) were
re-read for wrong answers: none found (the descriptor build pins `mn` / `pn` / the builder across every
allocating call; the module number is per (VM, loader) as `getDynamicModule`'s is per loader). The
closing step is unchanged and is a host run, not a lane's: the Spring `@CompileWithForkedClassLoader`
AOT tests on the Linux host with `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY=0` against the default
(identical: delete the block, the switch and its kind-map / flag rows; different: fix
`annotation_element_to_java_typed`'s `default_owner` loader instead). Status stays OPEN (item 4, host
run).
