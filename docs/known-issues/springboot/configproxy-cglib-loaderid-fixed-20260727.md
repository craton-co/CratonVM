# CGLIB `@Configuration` singleton-cache regression — FIXED 2026-07-27

## Summary

Root-caused and fixed the regression documented in
`docs/known-issues/springboot/configproxy-cglib-singleton-regression-20260727.md`
(`S03_ConfigProxy` scenario regressed 8/8 -> 4/8 in the real Spring Boot
functional suite; `engineBuiltOnce` expected the `@Bean` factory method to
run once, it ran 3 times — an inter-`@Bean`-method call on `this` was not
routing through the shared singleton, re-running the real factory body
every time).

## Root cause

`vm/src/vm/vm_exec.rs`'s `NativeContextImpl::define_class_full` decoded its
`loader_id: u32` parameter back into a `ClassLoaderId` with:

```rust
let cl_id = if loader_id == 0 { ClassLoaderId::Application } else { ClassLoaderId::UserDefined(loader_id) };
```

This is NOT the inverse of `loader_id_of_class`'s encoding
(`Bootstrap=0, Extension=1, Application=2, UserDefined(id)=id`) a few
hundred lines below it in the same file. `native-builtins/src/cglib_enhancer.rs`'s
`cce_enhance` fetches `super_loader_id = ctx.loader_id_of_class(super_class_id)`
(deliberately, so the generated `@Configuration` subclass is defined by the
SAME loader as its superclass — required for CGLIB-style enhancement to
work at all) and passes it straight into `define_class_full`. For a
superclass loaded by the real Application loader, this round-trip is
`Application -> 2 -> define_class_full -> UserDefined(2)` — a DIFFERENT
`ClassLoaderId` than `Application`.

`classloading/src/access_control.rs`'s `same_runtime_package` correctly
requires `a.loader_id == b.loader_id` per JVMS 5.4.4 (a runtime package is
`(defining loader, package name)`, not the name alone). With the enhanced
subclass mistagged `UserDefined(2)` against the superclass's real
`Application` tag, every package-private `@Bean` override (`engine()`,
`car1()`, `car2()` — none have an explicit access modifier, so all are
default/package-private per JLS) failed the
`same_runtime_package`-gated `is_true_override` check in
`classloading/src/class_manager.rs`'s `build_vtable_descriptors_with_overrides`.
Each override was therefore appended as a fresh, independent vtable slot
instead of replacing the inherited slot in place — so any ordinary
`invokevirtual` dispatch to e.g. `engine()` from within another `@Bean`
method's inherited body (`Cfg.car1()`, reached via the enhancer's own
`invokespecial super.car1()`) kept resolving to the SUPERCLASS's original
slot, never seeing the override, and re-ran the real (uncached) factory
body every time.

Confirmed empirically with a battery of targeted debug traces (later
removed) at every layer: the thread-local `SimpleInstantiationStrategy
.getCurrentlyInvokedFactoryMethod()` mechanism itself was working
correctly (confirmed via a standalone diagnostic probe printing its value
at each call site); an ISOLATED direct reflective re-invocation of the
override worked correctly (ruling out a general dynamic-class dispatch
bug); the vtable-population trace showed the exact mismatch:
`class_loader_id=UserDefined(2)` for the enhanced subclass vs.
`declaring_super_loader_id=Application` for the superclass, with
`same_runtime_package=false`.

Interestingly, `allocate_loader_id` (a few dozen lines below the fix, same
file) already documents this exact reserved-id invariant ("0=Bootstrap,
1=Extension, 2=Application... starting the counter at 3 guarantees every
allocated namespace is a genuine UserDefined id") from a prior, unrelated
Hibernate bytecode-enhancement fix — but that fix only addressed
NEWLY-ALLOCATED loader ids, not this DECODE site, which could still
misinterpret an EXISTING built-in loader's id when round-tripped back
through `define_class_full`.

## Fix

Made the decode the exact, symmetric inverse of the encode:

```rust
let cl_id = match loader_id {
    0 => ClassLoaderId::Application,  // preserve the pre-existing default-sentinel convention
    1 => ClassLoaderId::Extension,
    2 => ClassLoaderId::Application,
    other => ClassLoaderId::UserDefined(other),
};
```

(`0` intentionally keeps decoding to `Application`, not `Bootstrap`, to
avoid changing behavior for any existing caller that passes literal `0` as
a "default loader" sentinel rather than a genuine round-tripped
`loader_id_of_class` value — `Bootstrap` is not otherwise reachable through
this path in practice.)

Three sibling call sites in the same file had the identical bug shape and
were fixed the same way for consistency, though none were proven to cause
a currently-observed failure (verified safe via the full
`cratonvm-classloading` + `cratonvm-vm` test suites, both clean relative
to a pre-fix baseline):
- `define_class_with_loader` (previously ALWAYS `UserDefined(loader_id)`,
  no special-casing at all)
- `class_id_by_name_and_loader` / `class_id_defined_by_loader_exact`
  (same, always `UserDefined(loader_id)`)
- `list_initiated_class_ids` (same 2-arm bug as `define_class_full` had)

## Verification

- Standalone diagnostic (`CglibDiag.java`, prints
  `SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod()` at each
  `@Bean` method entry): `engineCtor` now stays at 1 (was 3), both cars
  share the same `Engine` instance.
- Real fixture: `S03_ConfigProxy` scenario at
  `/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`:
  **8/8** (was 4/8).
- Full 10-scenario Spring Boot suite: **10/10 scenarios, 95/95 checks**,
  no regressions (S01-S10 all still pass).
- `cargo test --release -p cratonvm-classloading --lib`: 637 passed, 0
  failed.
- `cargo test --release -p cratonvm-vm --lib`: 2410 passed, 18 failed --
  identical to the pre-fix baseline (confirmed by stashing the fix and
  re-running: same 18 `runtime::lock_order` tests fail both with and
  without this change, a pre-existing `--release`-vs-debug-assertion test
  config mismatch unrelated to this fix).
