# `spring-boot-http-codec`: `FilteredClassLoader` condition now honored

**Status: RESOLVED — 2026-07-18.**

`CodecsAutoConfigurationTests` had detected Jackson after Spring Boot had
hidden its packages with `FilteredClassLoader`. The failing method was
`kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable`.

## Resolution

The native `ClassLoader.loadClass` routing has direct-`URLClassLoader`
subclass coverage, so Spring Boot's protected `loadClass(String, boolean)`
filter override is selected. The remaining HTTP/Micrometer residual was in
the native replacement for Spring's `ClassUtils.forName(String, ClassLoader)`:
it treated a null loader as a global lookup. Spring's Java implementation
resolves null through the current thread context class loader.

`spring_class_utils_for_name_impl` now gets that context loader before taking
the user-loader path. This preserves filtering for `ClassUtils.isPresent`
with null and for explicit loaders.

## Validation

The focused Spring Boot closure passed all 116 tests across HTTP codec, JDBC,
Micrometer tracing, and R2DBC with JIT enabled and with `--nojit` on
2026-07-18.
