# Elasticsearch XContent provider ModuleDescriptor null

Status: fixed

Date observed: 2026-07-02

Date fixed: 2026-07-03

## Summary

Most current Elasticsearch suite failures came from `XContentProvider$Holder`
initialization under CratonVM. The provider path calls
`java.lang.Module.getDescriptor().uses()`, but CratonVM returned a null module
descriptor on this path.

Failure signature:

```text
java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: Cannot invoke
"java.lang.module.ModuleDescriptor.uses()" because the return value of
"java.lang.Module.getDescriptor()" is null
```

Many classes then also reported:

```text
java.lang.NoClassDefFoundError: org/elasticsearch/xcontent/XContentType
```

## Root cause (two compounding bugs)

**Bug 1 — `Module.getDescriptor()` never populated.** `java.lang.Module` loads
as real JDK bytecode under CratonVM. Real HotSpot guarantees
`isNamed() == (getDescriptor() != null)` — a named module's descriptor is
never null. CratonVM's `isNamed()` (real bytecode, reading the dual-written
real `name` field) can report a classpath-loaded, modularized jar as named,
but `getDescriptor()`'s real bytecode (`return this.descriptor;`) read a
field CratonVM never populated. Elasticsearch's
`ProviderLocator.checkUses` — `caller.isNamed() && caller.getDescriptor()
.uses()...` — hit this directly.

Fix: force `Module.getDescriptor()` to a native (alongside the existing
`isExported`/`isOpen` overrides in `force_native_over_real_jdk_bytecode`,
`../../../../vm/src/runtime/interpreter.rs`) that returns null only for the true unnamed
module and otherwise builds a descriptor backed by the boot `ModuleRegistry`'s
parsed `uses` (`native-builtins/src/lib.rs::register_essential_natives`).

**Bug 2 (the actual blocker) — a field-layout collision corrupted the shared
unnamed-module mirror before bug 1's fix ever ran.** `native-builtins/src/
jboss_jdkspecific.rs` registers `Module.getName()`/`getLayer()` **unconditionally
for every app** (not just WildFly/JBoss), assuming a private 5-field synthetic
Module layout (`name=0, layer=1, packages=2, descriptor=3, loader=4`). But
`alloc_concurrent_synthetic` resolves `"java/lang/Module"` to the real
bytecode class, whose actual layout is `layer=0, name=1, loader=2,
descriptor=3` — the reverse of what that file assumed for the first two
fields.

`Class.getModule()` (`../../../../native-builtins/src/lib.rs`) caches ONE canonical Module
mirror per module name — including a single shared mirror for the unnamed
module (JDK identity-compares Modules, so every class with no declared module
must observe the same instance). Any call to `Module.getLayer()` anywhere
during boot — on ANY Module object, not necessarily one related to
Elasticsearch — wrote a `ModuleLayer` object into what is actually the real
`name` field of that shared instance. From that point on, every subsequent
`Module.getDescriptor()` call on the (still-cached, still-shared) unnamed
module read back a `ModuleLayer` where it expected the module name, so
`module_name_of_mirror` resolved to `""` (empty) — meaning bug 1's fix, once
applied, still returned null for a class that genuinely appeared named.
Confirmed by direct tracing: `native_module_get_layer` fired once on
`ClassId(541)` (`java/lang/Module`) with `existing_slot1=None`, immediately
followed by the shared unnamed-module cache entry showing
`slot1=Some(ModuleLayer)`.

Fix: `build_module`/`native_module_get_name`/`native_module_get_layer` now
resolve `"name"`/`"layer"` by real field name (`get_field_by_name`/
`set_field_by_name`) instead of hardcoded slot indices, so they agree with
every other code path touching these two fields regardless of which one
constructed the object. `native_module_get_packages`' slot 2 (aliasing the
real `loader` field) is left as a documented residual — not implicated in
this failure, but the same class of bug; a `Module.getPackages()` call on a
foreign-layout Module object would corrupt `loader` the same way.

## Fix commits

Branch `fix/module-getdescriptor-uses-null-20260703` (3 commits):

- `af51f317` — `fix(module): populate Module.getDescriptor() instead of
  returning null` (bug 1: `../../../../native-api/src/registry.rs`,
  `../../../../native-builtins/src/lib.rs`, `../../../../native-builtins/src/phases_late.rs`,
  `../../../../vm/src/runtime/interpreter.rs`, `../../../../vm/src/vm/vm_exec.rs`)
- `6717d333` — `fix(module): stop JBoss-Modules Module natives from
  corrupting real Module field layout` (bug 2:
  `../../../../native-builtins/src/jboss_jdkspecific.rs`, `Module.getName()`/`getLayer()`)
- `b0b6e545` — `fix(module): stop Module.getPackages() aliasing the real
  loader field` (bug 3, same file, `Module.getPackages()` — see below;
  not confirmed live in the original XContentProvider chain, found while
  auditing the rest of this file for the same bug class)

Bug 1's fix alone did **not** resolve the Elasticsearch failures — bug 2 had
to be found and fixed too. This was confirmed the hard way: after bug 1
landed, the exact same NPE with the exact same signature still reproduced;
extensive field-level tracing (dumping raw field slots, resolved field
indices, and the concrete class of the object stored in the "name" field) is
what surfaced bug 2.

**Bug 3 — `Module.getPackages()`, same collision, different field, plus a
second nested bug it exposed.** `native_module_get_packages` hardcoded field
slot 2 for the package list. Real `java.lang.Module` has no field literally
named "packages" (real `getPackages()` is computed, not stored); slot 2 is
actually the real `loader` field. Any Module object not built by this file's
own `build_module` (e.g. the same canonical shared unnamed-module mirror
implicated in bug 2) would have its real `loader` field silently overwritten
with a `Set` the first time `getPackages()` was called on it. Confirmed live
via a direct Java-level probe (`getModule().getPackages()` does dispatch to
this native in the default build, despite `getPackages` not being in
`force_native_over_real_jdk_bytecode` — reachability isn't only decided by
that list). Fixed by moving the package list off-object into an
identity-hash-keyed side table (`module_packages_table`, same bounded pattern
as `mac_state_table` in `phases_late.rs`), storing a plain `Vec<String>`
rather than a `java.util.Set` reference to avoid needing a GC root for a
value cached across native calls.

Verifying bug 3 end-to-end (calling `.getPackages().size()` from Java)
surfaced a **second, independent bug**: `build_package_set` built the
returned `HashSet` using a hand-rolled `(array, size, capacity)` synthetic
layout, but real `java.util.HashSet` has exactly one field
(`transient HashMap<E,Object> map`) — the mismatch corrupted real
`size()`/`iterator()`/`stream()` bytecode, observed directly as `.size()`
always returning `0` plus `gen_heap::read_slot: corrupt Value cell`
GC-guard errors in the log. Fixed by switching to the existing, already
-correct `build_real_layout_string_hashset` helper (`native-builtins/src/
lib.rs`), used elsewhere for the identical bug class (its own `S111r11+`
comment on `Properties.stringPropertyNames` describes fixing the same thing
for a different caller). Verified via direct probe: package-set size now
correctly matches `BOOT_JDK_PACKAGES`'s length (63) with no corruption
errors, for both a real named module (`java.base`) and the unnamed module.

`native_module_get_packages`'s `HashSet` layout bug was general — any other
caller elsewhere in the codebase building a `HashSet` via the same
`(array, size, capacity)` convention on a class whose real bytecode is
loaded (rather than a fully synthetic stand-in) could hit the identical
corruption. Not audited beyond this file's own `build_package_set`.

## Verification

Repro (same repro row the bug was originally filed against, plus the two
classes independently reported alongside it):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 429 -Count 6 -Parallel 1 -TimeoutSec 300 `
  -RunName modgetdesc-fix3-FINAL `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-suite-runner-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-modgetdesc-fix3\target\release\cratonvm-modgetdesc-fix3.exe
```

Result — the `ModuleDescriptor.uses()` NPE signature is gone from all 6
classes (`grep -c "ModuleDescriptor.uses"` == 0 in every `.err.log`):

```text
PASS   Retry2Tests
PASS   RetryTests
PASS   ShardBatchIndexerCanUseBatchTests
FAIL   ShardBatchIndexerTests            (unrelated: NoClassDefFoundError NodeRoleSettings)
FAIL   ShardBatchMapperParseTests        (unrelated: NoClassDefFoundError NodeRoleSettings)
PASS   ShardBatchMapperResolveTests
```

`EcsJsonUtilsTests` (the doc's original representative repro row, module
`libs/cli-terminal`) also has the NPE signature gone; it now progresses past
`XContentProvider$Holder.<clinit>` entirely and fails on an unrelated,
previously-masked bug:

```text
java.lang.NoSuchMethodError: com/fasterxml/jackson/core/StreamReadConstraints
$Builder.maxNameLength(I)Lcom/fasterxml/jackson/core/StreamReadConstraints$Builder;
```

Both newly-exposed failures (`NodeRoleSettings` `NoClassDefFoundError`, the
Jackson `StreamReadConstraints` `NoSuchMethodError`) are separate,
pre-existing CratonVM gaps that this NPE was masking — not regressions from
this fix. They are not yet filed as their own known-issues docs.

Re-ran the same 6-class repro after bug 3's fix (`b0b6e545`) landed on top —
identical results, confirming no regression:

```text
PASS   Retry2Tests
PASS   RetryTests
PASS   ShardBatchIndexerCanUseBatchTests
FAIL   ShardBatchIndexerTests            (unrelated: NoClassDefFoundError NodeRoleSettings)
FAIL   ShardBatchMapperParseTests        (unrelated: NoClassDefFoundError NodeRoleSettings)
PASS   ShardBatchMapperResolveTests
```

Full `native-builtins` unit test suite after all 3 commits: 2749 passed, 0
failed, 5 ignored (unchanged from before this branch).

## Historical repro (pre-fix)

Original full-suite scope, for reference:

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 2273 CratonVM failures contained this signature.
- 2171 were CratonVM-only: HotSpot passed the same classes.
- 87 overlapped HotSpot baseline failures.
- 13 overlapped HotSpot baseline crashes.
- 2 overlapped HotSpot baseline hangs.

Representative row:

```text
index=19
module=libs/cli-terminal
class=org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests
CratonVM=FAIL, 29.156s
HotSpot=PASS, 8.556s
```

Other CratonVM-only examples:

```text
org.elasticsearch.cli.terminal.TerminalTests
org.elasticsearch.cli.terminal.JsonTerminalTests
org.elasticsearch.common.collect.TupleTests
org.elasticsearch.common.CharArraysTests
org.elasticsearch.common.unit.TimeValueTests
```

No-JIT partial run `es-nojit-full-20260702` (stopped after 1366 recorded
classes): 1194 failures contained this signature, 1140 CratonVM-only.

A full-suite re-run to quantify the total improvement across all ~2701
classes has not been done yet — the verification above is targeted at the
classes named in this doc and the original bug report, not the entire suite.

## Evidence paths

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.out.log
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-suite-runner-20260702\apps\elasticsearch-suite-runner\.suite\results\modgetdesc-fix3-FINAL\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-suite-runner-20260702\apps\elasticsearch-suite-runner\.suite\results\modgetdesc-fix3-FINAL-ecs2\all-jit\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.err.log
```
