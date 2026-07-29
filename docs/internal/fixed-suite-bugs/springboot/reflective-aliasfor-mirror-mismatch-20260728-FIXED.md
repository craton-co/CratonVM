# Resolved: `@Reflective` `@AliasFor` mirror mismatch during Spring bean post-processing

**Status: FIXED — 2026-07-29**

## Symptom

`MultipartAutoConfigurationTests` failed while Spring scanned a
`@RequestMapping` controller. Spring reported that `@Reflective.processors`
and `@Reflective.value` had different `@AliasFor` mirror values.

## Fix

When an annotation proxy materializes Spring's `@Reflective`, CratonVM now
canonicalizes the reciprocal `value`/`processors` pair. If one member was
explicitly supplied and the other omitted, both proxy members use the explicit
value before Spring validates the mirror pair. This prevents a loader-specific
default `Class` mirror from making an otherwise equivalent alias pair fail
identity-sensitive validation.

The complete class then exposed a separate socket residual in
`webServerWithNothing`: JDK blocking sockets can supply an internal descriptor
outside CratonVM's NIO-handle registry when applying advisory TCP keepalive
settings. `TCP_KEEPIDLE` writes now remain real `setsockopt` calls for
VM-owned descriptors and succeed as no-ops for that opaque descriptor path,
matching the non-failing JDK socket configuration behavior.

## Verification

Fresh JDK 25 release binary; complete 12-case
`module/spring-boot-servlet` `MultipartAutoConfigurationTests`:

| Mode | Result |
|---|---|
| JIT enabled | 12 passed, 0 failed (55.507s) |
| `--nojit` | 12 passed, 0 failed (58.644s) |

This record was moved from `docs/known-issues` after both complete runs
passed.
