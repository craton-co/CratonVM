# CGL.1 (standalone/unshaded CGLIB) - RETIRED 2026-07-29

**Status**: fixed by re-verification; the `net/sf/cglib/` JIT ban stays lifted.

## Scope

This covers only the standalone, unshaded `net.sf.cglib` artifact.  It does
not cover Spring's independently packaged `org.springframework.cglib` copy.

The original report had become unverifiable because its only fixture had been
deleted.  Current `dev` already has no active `net/sf/cglib/` skip-list branch;
the formerly broad CGL.1 block is entirely commented out.  This closure
restores the fixture as `apps/cglib_probe` so the ban remains continuously
verifiable without a package-allow override.

`CglibProbe` uses CGLIB 3.3.0 and ASM 7.1 to create an `Enhancer` subclass of
`Greeter`, install a `MethodInterceptor`, invoke the generated proxy, and
require the exact result `hello world (cglib)`.

## Azure validation

Validated against `origin/dev` (`c31344865`) on 2026-07-29 using real Java 25
(`/home/victor/jdk25`) and an isolated release binary.

| Runtime | Configuration | Result |
| --- | --- | --- |
| HotSpot Java 25 | baseline | 1/1 PASS |
| CratonVM | default JIT | 20/20 PASS |
| CratonVM | `CRATONVM_JIT_THRESHOLD=1` | 20/20 PASS |
| CratonVM | `CRATONVM_DISABLE_JIT=1` | 20/20 PASS |

All 60 CratonVM processes printed `CglibProbe: PASS` and exited normally.  No
process used `CRATONVM_JIT_ALLOW_PACKAGES`; no SIGSEGV, proxy-wiring
`IllegalStateException`, incorrect interceptor result, or timeout occurred.

The committed runner is:

```text
apps/cglib_probe/run-cglib-probe.sh <cratonvm> <cglib-jar> <asm-jar> <jit|nojit> [runs]
```

This document moved from `docs/known-issues/jit-bans` after the exact
standalone-CGLIB witness was green in both JIT and interpreter modes.