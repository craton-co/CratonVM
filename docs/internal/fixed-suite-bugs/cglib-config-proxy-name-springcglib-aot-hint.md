# Synthetic `@Configuration` Proxy AOT Hint Name

**Status:** FIXED 2026-07-01 on branch `codex/jit-known-issues-20260701-4`.
**Mode:** real-JDK, JIT on. HotSpot passes.
**Original dev verification basis:** `ecfe859f`.

## Symptom

`org.springframework.context.annotation.AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`
expected an AOT reflection hint on `<Config>$$SpringCGLIB$$0`:

```java
context.register(CglibConfiguration.class);
RuntimeHints runtimeHints = new RuntimeHints();
context.refreshForAotProcessing(runtimeHints);
TypeReference cglibType = TypeReference.of(CglibConfiguration.class.getName() + "$$SpringCGLIB$$0");
assertThat(RuntimeHintsPredicates.reflection().onType(cglibType).withMemberCategories(
        INVOKE_DECLARED_CONSTRUCTORS, INVOKE_DECLARED_METHODS, ACCESS_DECLARED_FIELDS))
    .accepts(runtimeHints);
```

The member categories were already identical to HotSpot. The only difference was
the proxy class name used as the hint key:

| VM | proxy type registered |
| --- | --- |
| HotSpot | `CglibConfiguration$$SpringCGLIB$$0` |
| CratonVM before fix | `CglibConfiguration$$EnhancerByCGLIB$$0` |

Real Spring registers the hint from `enhancedClass.getName()`, so it landed under
whatever name CratonVM's synthetic enhancer exposed through `Class.getName()`.

## Root Cause

CratonVM's synthetic `@Configuration` enhancer
(`native-builtins/src/cglib_enhancer.rs`, `build_enhancer_class`) deliberately
uses an internal name of this form:

```text
<Config>$$EnhancerByCGLIB$$<hex>
```

That avoids colliding with real Spring CGLIB, which may later define runtime AOP,
scoped, or generics proxies named:

```text
<Config>$$SpringCGLIB$$<n>
```

Renaming the actual synthetic class to `$$SpringCGLIB$$0` made this one AOT test
pass, but caused duplicate class-definition failures when the same configuration
class also needed a real Spring CGLIB proxy.

## Fix

The real synthetic class keeps the collision-free `$$EnhancerByCGLIB$$` internal
name. The fix is in `native-builtins/src/lang_class.rs`: `Class.getName()` now
exposes the HotSpot-style AOT hint display name only when both conditions hold:

- the internal class name contains `$$EnhancerByCGLIB$$`;
- the class directly implements Spring's
  `ConfigurationClassEnhancer$EnhancedConfiguration` marker interface.

For those synthetic configuration classes, `Class.getName()` reports:

```text
<Config>$$SpringCGLIB$$<n>
```

Ordinary real CGLIB proxies that also contain `$$EnhancerByCGLIB$$`, but do not
directly implement the Spring enhanced-configuration marker, keep their real
binary names. This fixes the AOT reflection-hint key without reintroducing the
class-definition collision caused by renaming the actual synthetic class.

## Validation

Added focused regression tests:

- `class_get_name_spring_configuration_cglib_uses_aot_hint_alias`
- `class_get_name_regular_cglib_proxy_keeps_actual_name`
- `class_get_name_spring_configuration_cglib_alias_formats_hex_counter_as_decimal`

Command run:

```powershell
cargo test -p cratonvm-native-builtins class_get_name --lib
```

Result: 5 matching `Class.getName` tests passed. A first attempt with a fresh
`CARGO_TARGET_DIR=target\codex-jit-20260701-4` failed before Rust compilation in
`libffi-sys`'s Windows assembler build; rerunning against the warmed workspace
target completed successfully.

The full Spring Framework harness was not available in this worktree, so this
fix is validated at the native surface where the suite failure diverged.
