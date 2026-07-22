# `ResourceProviderCustomizerBeanRegistrationAotProcessorTests` - fixed AOT substitution

**Status: FIXED - 2026-07-18**

## Symptom

`module/spring-boot-flyway`'s
`ResourceProviderCustomizerBeanRegistrationAotProcessorTests` generated and
executed an AOT context, but its `ResourceProviderCustomizer` bean remained
the plain type instead of `NativeImageResourceProviderCustomizer`.

## Root cause

Spring's `TestCompiler` runs the configuration through a forked class loader.
While CratonVM built a reflective `Method` mirror for the configuration's
factory method, its descriptor return type was resolved from the global
application namespace. The factory bytecode itself resolved
`ResourceProviderCustomizer` through the forked loader. The two same-named
classes therefore had different identities, causing Spring's exact
`registeredBean.getBeanClass().equals(ResourceProviderCustomizer.class)`
check to skip the AOT processor silently.

## Fix

`descriptor_to_class_mirror_via_loader` now consults the authoritative
defining-loader side table. If the referenced class is not already in that
loader's exact namespace, it invokes that loader's virtual
`loadClass(String)` implementation before retaining the legacy global
descriptor fallback. This preserves custom one-argument `loadClass`
overrides used by Spring's forked loader. The native path pins the loader and
class-name objects over the potentially allocating, re-entrant call.

The helper is shared by reflective method, field, and constructor descriptor
resolution, so the fix covers the same loader-identity mismatch throughout
those reflection paths.

## Validation

- Release build: `cargo build --release` completed successfully (the final
  binary used a unique `cratonvm-sb-flyway-aot-final-20260717.exe` name).
- Focused Spring Boot runner acceptance with the exact three-test class:
  `ResourceProviderCustomizerBeanRegistrationAotProcessorTests` passed 3/3
  with JIT enabled (60.183 s) and 3/3 with `--nojit` (57.373 s).
- The Spring Boot fixture checkout was clean after the run; the VM fix does
  not modify application or test sources.
