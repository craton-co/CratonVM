# Keycloak model tests: Infinispan GlobalConfiguration.isClustered() NoSuchMethodError

| | |
|---|---|
| **Status** | ✅ **FIXED** on branch `fix/kc-infinispan-isclustered-nosuchmethod-20260706` (`../../../../native-builtins/src/infinispan_local.rs`). |
| **Area** | Infinispan local-mode cache natives (`register_infinispan_natives`) — the `GlobalConfigurationBuilder.build()` identity-wrapper native. |
| **Severity** | was high — blocked all 37 `testsuite/model` classes identically at `KeycloakModelTest.<clinit>`. |
| **Discovered** | 2026-07-03. |
| **Fixed** | 2026-07-06. |

## Root cause

**Not** a class-identity / vtable / method-resolution bug in the interpreter or
JIT, despite the misleading symptom. `invokevirtual` dispatch, constant-pool
resolution (`resolve_method_ref`), the `ResolutionCache` (keyed by
`(ClassId, cp_index)`, not by name/hash), `find_method_recursive`, and
`find_class_by_name`/`get_loaded_class_id` (all exact-string-match, no
prefix/hash-collision path) were all audited and found correct.

The actual bug: `../../../../native-builtins/src/infinispan_local.rs` registered a native
override for `GlobalConfigurationBuilder.build()`
(`native_global_cfg_build`) as a pure **identity wrapper** — it returned `this`
(the `GlobalConfigurationBuilder` receiver itself) instead of constructing a
genuinely distinct `GlobalConfiguration` object:

```rust
// GlobalConfigurationBuilder.build() — same identity wrapper.
fn native_global_cfg_build(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(Value::Object(Some(this))))
}
```

This native was registered **unconditionally** in `register_infinispan_natives`
(no `if !real_infinispan()`-style gate, unlike the `real_agroal()` /
`real_vertx()` precedent elsewhere in the same file) and, per CratonVM's
native-override-priority rule (a registered native always wins over available
real-JDK bytecode for the same class+method+descriptor), it shadowed the real,
correct `GlobalConfigurationBuilder.build()` bytecode — even though the real
`infinispan-core-16.0.8.jar` was on the classpath and its bytecode is entirely
correct.

Confirmed directly with an isolated probe
(`new GlobalConfigurationBuilder().build()`): the returned object's
`getClass().getName()` was `org.infinispan.configuration.global.
GlobalConfigurationBuilder` — the exact same object, just relabeled as the
return type — instead of a genuine `GlobalConfiguration`. Since
`GlobalConfigurationBuilder` (43 methods, confirmed via `javap`) genuinely has
no `isClustered()` method — only `GlobalConfiguration` (confirmed via `javap`:
`public boolean isClustered();`) does — any caller that invoked
`isClustered()` on the object the shim handed back hit a **real**
`NoSuchMethodError` correctly naming `GlobalConfigurationBuilder`, because
that receiver's runtime class genuinely was `GlobalConfigurationBuilder`. The
interpreter's dispatch was reporting the truth about a corrupted object
identity, not misattributing a class name.

`org.infinispan.configuration.serializing.CoreConfigurationSerializer.
writeJGroups` is exactly this call site in real Infinispan: its second
parameter (and the `invokevirtual` receiver at `pc=1`) is declared
`GlobalConfiguration`, and the real bytecode's constant pool entry
unambiguously references `GlobalConfiguration.isClustered:()Z` (confirmed via
`javap -c -v` against `CoreConfigurationSerializer.class`) — there was never
any ambiguity in the bytecode itself. Every `testsuite/model` class reaches
this same `KeycloakModelTest.<clinit>` → `DefaultInfinispanConnectionProviderFactory`
→ Infinispan cache-manager bootstrap → config serialization path, so the
crash reproduced identically across all 37 classes.

## Fix

Removed `native_global_cfg_build` and its registration on
`GlobalConfigurationBuilder.build()`. Real bytecode now runs and constructs a
genuinely distinct `GlobalConfiguration` instance, matching real-JDK/HotSpot
behavior exactly.

Verified this is safe to un-shim (unlike the sibling `ConfigurationBuilder` /
`Configuration` pair — see "Residual" below): nothing in
`../../../../native-builtins/src/infinispan_local.rs` reads `GlobalConfiguration`'s fields
by the synthetic slot-index layout (`GC_FIELD_SITE_NAME` / `GC_FIELD_JMX_ENABLED`
/ `GC_FIELD_RESERVED` are declared in the field-layout table but never read
anywhere in the file), and `native_dcm_init` (`DefaultCacheManager`'s
constructor native, the only place a `GlobalConfiguration` argument is
consumed) ignores its `GlobalConfiguration` parameter entirely.
`ConfigurationBuilder.build()`'s identical-shaped identity wrapper
(`native_cfg_build`) was deliberately left in place: `native_dcm_
define_configuration` reads `CONFIG_FIELD_SIZE_LIMIT` / `CONFIG_FIELD_TTL_MS`
off its `Configuration` argument by raw synthetic slot index, so letting real
bytecode run there would return a real, differently-laid-out `Configuration`
object and silently corrupt (or panic on) those reads. See "Residual" below.

## Verification

- **Isolated probe** (`new GlobalConfigurationBuilder().build().isClustered()`
  against the real `infinispan-core-16.0.8.jar`, full `testsuite/model` Maven
  classpath): before the fix, `build() OK, class=…GlobalConfigurationBuilder`
  then `NoSuchMethodError: …GlobalConfigurationBuilder.isClustered()Z`. After
  the fix: `build() OK, class=…GlobalConfiguration`, `isClustered() = false`,
  `PROBE PASS` — matches real JDK 25 HotSpot exactly (verified side-by-side on
  the same classpath).
- **Full repro** (`run-keycloak-suite.ps1`, `RealmModelTest`, `-Vm craton -Jit
  on`): before the fix, `CRASH` — `no class def found:
  org/keycloak/testsuite/model/KeycloakModelTest` (the `NoSuchMethodError`
  propagating as an uncaught VM-level linkage failure). After the fix: `FAIL`
  — a real Java-level `ExceptionInInitializerError` /
  `ClassCastException`, i.e. the VM now runs to a genuine JUnit outcome
  instead of crashing. The `isClustered()` `NoSuchMethodError` no longer
  appears anywhere in the log.
- Rust regression test added:
  `classloading/src/class.rs::tests::find_method_recursive_does_not_confuse_similarly_named_sibling_classes`
  — pins the general invariant (two unrelated, similarly-named sibling
  classes; only one declares a given method; resolving on the declaring class
  succeeds and is never satisfied by the sibling) so a REAL regression in
  `find_method_recursive` would be caught, even though that function was never
  actually the culprit here.
- `cargo test --release -p cratonvm-classloading` and
  `-p cratonvm-vm --lib`: no regressions from this change (one pre-existing,
  unrelated failure — `verifier::tests::concrete_class_missing_abstract_impl_rejected`
  — reproduces identically on the pre-fix baseline, confirmed via `git stash`).
  `cargo test --release -p cratonvm-native-builtins infinispan`: 20/20 passed,
  both before and after this change.

## Residual (new, follow-up needed)

Fixing this bug unmasked a **second, closely related** identity-wrapper bug
one step deeper in the same real-Infinispan code path:
`org.infinispan.configuration.cache.ConfigurationBuilder.build()` has the
exact same shape (`native_cfg_build`, deliberately left in place — see "Fix"
above). Once `GlobalConfigurationBuilder.build()` runs real bytecode,
`CoreConfigurationSerializer.writeCacheContainer` reaches further and does
`(Configuration) configurationBuilder.build()`, which now throws:

```
java.lang.ClassCastException: org.infinispan.configuration.cache.ConfigurationBuilder
cannot be cast to org.infinispan.configuration.cache.Configuration
```

This was unreachable before this fix (masked by the earlier crash) and is a
distinct, separately-scoped fix: `native_dcm_define_configuration` must stop
reading `Configuration`'s fields by raw synthetic slot index before
`ConfigurationBuilder.build()` can be safely un-shimmed the same way. Tracked
as a follow-up, not fixed here.

## Repro (pre-fix, for reference)

```powershell
$list = "C:\temp\keycloak-model-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list -Category others -Vm craton -Jit on -Parallel 1 -TimeoutSec 300 `
  -RunName kcmodel-isclustered-fix-verify `
  -KeycloakRoot "<keycloak-checkout>" `
  -WorkDir "apps\keycloak-suite-runner\.suite" `
  -Exe "target\release\cratonvm.exe"
```

## Cross-reference

Supersedes `docs/known-issues/keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md`
(moved here, fixed).
