# ES FIX — `EmbeddedImplClassLoader` nested-jar class and resource loading

Status: FIXED 2026-07-17

## Problem

Elasticsearch packages implementation dependencies below
`IMPL-JARS/<module>/<jar>/...` and loads them through
`EmbeddedImplClassLoader`. CratonVM did not consistently preserve those
per-loader archives, multi-release entries, and module-aware definitions,
which surfaced as `NoClassDefFoundError` during `XContentType` bootstrap.

The focused loader suite also exposed a residual Java-contract defect:
after a non-null inherited `ClassLoader` call populated an inline cache,
`getResource`, `getResources`, `getResourceAsStream`, `resources`, and
`loadClass` accepted a later null name instead of throwing
`NullPointerException`.

## Resolution

The embedded-loader implementation now exposes nested `IMPL-JARS` archives
and multi-release/module metadata through the defining loader. The
interpreter additionally evicts only the affected cached inherited
`ClassLoader` call when its name argument is null, then invokes the
canonical native contract. `loadClass` now creates a catchable Java
`NullPointerException`, rather than a VM-internal error.

Normal non-null resource lookup remains cacheable and unchanged.

## Verification

Built from the isolated `dev` worktree using the unique executable
`cratonvm-es-embeddedimplclassloader-20260717.exe` and Java 25:

- `EmbeddedImplClassLoaderTests` (19 tests): PASS with JIT on — 189.5 s.
- `EmbeddedImplClassLoaderTests` (19 tests): PASS with JIT off — 329.1 s.
- The regression includes all five null-name `ClassLoader` checks and the
  nested-jar/multi-release coverage that originally reproduced the issue.

Result directories:

- `C:\craton\es0717\results\es-embeddedimplclassloader-final-r44-20260717\final-r44-jit`
- `C:\craton\es0717\results\es-embeddedimplclassloader-final-r45-20260717\final-r45-nojit`
