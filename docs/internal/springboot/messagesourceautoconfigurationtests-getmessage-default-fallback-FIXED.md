# `MessageSourceAutoConfigurationTests` — dynamic message bundles ignored (FIXED)

**Status: FIXED — 2026-07-18**

## Symptom

Six `MessageSourceAutoConfigurationTests` cases returned the literal caller
default (`"Foo message"`) rather than the configured bundle value, or threw
`NoSuchMessageException` when no default was supplied. The affected tests use
the Spring Boot `@WithResource` test extension to make temporary
`test/*.properties` files visible through the thread context class loader.

## Root cause

CratonVM replaces every `ResourceBundle.getBundle` overload in
`native-builtins/src/locale_resources.rs`. Its property-bundle candidate loop
read only `NativeContext::find_resource`, the VM-wide application class path.
It ignored the explicit `ClassLoader` argument accepted by the
`(String, Locale, ClassLoader[, Control])` overloads.

`ResourceBundleMessageSource` passes the thread context loader to that
overload. Spring's `ResourcesExtension` creates a `ResourcesClassLoader` over
a per-test temporary directory, so the global lookup could never see its
generated `test/messages.properties`, `test/messages2.properties`, or
`test/swedish.properties`. The regular Spring fallback behavior then correctly
returned the caller default or raised `NoSuchMessageException`.

## Fix

`rb_get_bundle` now identifies an explicit `ClassLoader` argument and resolves
each `.properties` candidate through its `getResourceAsStream`, reading the
returned stream before parsing the properties. This preserves the loader's
parent-first delegation and custom `findResource` implementation. Overloads
without a loader retain the existing VM-wide class-path lookup; an explicit
loader does not accidentally leak resources from an unrelated loader.

## Verification

On the Azure Linux host, using the Spring Boot 4.1.0-SNAPSHOT fixture and real
JDK 25, regenerated the Linux test runtime class path and ran
`org.springframework.boot.autoconfigure.context.MessageSourceAutoConfigurationTests`
through `SbRunner`:

| Runtime | Result |
|---|---|
| HotSpot JDK 25 | `tests=19 failed=0 aborted=0 skipped=1` |
| CratonVM JIT | `tests=19 failed=0 aborted=0 skipped=1` |
| CratonVM `--nojit` | `tests=19 failed=0 aborted=0 skipped=1` |

The unique validation binary was
`/data/cvm-messagesource-fallback-20260718` (SHA-256
`6156d4398d6018c0ec6bbd170de4cfc092aa56e8fc0289199fc24eb847673173`).
The expected warning for the test that deliberately requests a missing common
messages file occurred in all runs and was asserted by that test; it is not a
failure.
