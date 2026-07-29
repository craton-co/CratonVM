# SPB.9b's remaining 2 sub-bans (Spring Boot loader, WebFlux) — removed 2026-07-27, UNVERIFIED

**Status**: removed (code deleted, not just commented out — this was
addressed a day before the broader "comment out non-target-app bans"
pass), no re-verification. Per explicit user decision. **This is the
highest-exposure removal in the whole 2026-07-27/28 sweep** —
`org/springframework/boot/loader/` is exercised by essentially every
packaged, executable Spring Boot fat jar.

## What it banned

Two blanket package prefixes (the third sibling sub-ban,
`org/springframework/beans/factory/support/`, was independently
re-verified and removed on 2026-07-26 with real evidence — see the
consolidated tracker):

- `org/springframework/boot/loader/` (Spring Boot's `JarLauncher`/
  `LaunchedURLClassLoader`)
- `org/springframework/web/reactive/` + `org/springframework/boot/web/reactive/`
  (WebFlux reactive handler chains)

## Original symptom (Session 114)

After SPB.9 pinned the per-class logger wiring, the next downstream
allocate-then-putfield consumers on `insurance-backend`'s (never found
on this host) `prepareEnvironment` → component-scan critical path were:

- **`org/springframework/boot/loader/`**: `JarLauncher`/
  `LaunchedURLClassLoader` allocate per-jar `Archive`/`Source` records
  and store them via putfield. This ban intentionally overrode the
  SPB.4c `loader/` exemption because `JarLauncher.launch` was itself the
  entry point that failed dispatch.
- **`org/springframework/web/reactive/` + `org/springframework/boot/web/reactive/`**:
  `insurance-backend` used Spring WebFlux; `ReactiveWebServerApplicationContext`
  and `ReactiveWebServerFactory` allocate Reactor Netty handler chains
  (`HttpHandler`, `WebFilter`) whose constructors store config slots
  immediately after `new`.

## Why it was never re-verified before being removed

The original fixture app (`insurance-backend`) was never found on this
host — exhaustively searched at full filesystem depth (by content and
purpose, not just name) twice, independently, by two concurrent
sessions. There is no known equivalent real, executable Spring Boot fat
jar or real WebFlux-booting app on this host to substitute.

## How to restore

In `vm/src/jit/skip_list.rs`, find the `SPB.9b -- ALL THREE sub-bans now
removed` comment inside `should_skip_jit_internal` and re-add:

```rust
if class_name.starts_with("org/springframework/boot/loader/")
    && !package_allowed("org/springframework/boot/loader/", allow_packages)
{
    return Some(SkipReason::RustJvmTestFixture);
}
if class_name.starts_with("org/springframework/web/reactive/")
    && !package_allowed("org/springframework/web/reactive/", allow_packages)
{
    return Some(SkipReason::RustJvmTestFixture);
}
if class_name.starts_with("org/springframework/boot/web/reactive/")
    && !package_allowed("org/springframework/boot/web/reactive/", allow_packages)
{
    return Some(SkipReason::RustJvmTestFixture);
}
```

## Repro (for whoever re-verifies)

- **JarLauncher**: build a real, executable Spring Boot fat jar (`java
  -jar app.jar`) and boot it under default JIT tiering, watching for a
  SIGSEGV during `JarLauncher.launch`.
- **WebFlux**: boot a real Spring Boot WebFlux (reactive) application
  under default JIT tiering, watching for corruption in
  `HttpHandler`/`WebFilter` construction during
  `ReactiveWebServerApplicationContext` startup.

Note: the real 10-scenario Spring Boot suite this session used
(`spring-boot-tomcat-crossmodule-20260717`) does **not** exercise either
of these two code paths (it uses `AnnotationConfigApplicationContext`
directly, not a packaged fat jar or WebFlux), so its 10/10 pass rate
provides **no** evidence either way for this specific removal.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Removed 2026-07-27, UNVERIFIED" section) and the corresponding unit
test `beans_factory_support_is_jit_eligible_after_spb9b_full_removal` in
`vm/src/jit/skip_list.rs`.
