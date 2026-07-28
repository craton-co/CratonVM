# CGL.1 (standalone/unshaded CGLIB) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision.

## What it banned

`net/sf/cglib/` (blanket package prefix) — the **standalone, unshaded**
CGLIB artifact. Spring's own internal, repackaged copy
(`org/springframework/cglib/`) is a completely separate package and is
**unaffected** by this ban either way.

## Original symptom (Session 117, agent O4, 2026-05-16)

`apps/cglib_probe` (a minimal repro: `Enhancer.create()` over `Greeter`
with a single `MethodInterceptor` lambda, never found on this host in
later sessions) SEGFAULT'd (rc=139) immediately after `<clinit>` of the
generated proxy class
(`CglibProbe$Greeter$$EnhancerByCGLIB$$<hash>`). With
`CRATONVM_DISABLE_JIT=1` the SEGFAULT disappeared and the run surfaced a
clean `IllegalStateException` (a separate, downstream proxy-wiring gap,
not a JIT issue) — a classic JIT-miscompile signature. CGLIB's hot boot
path (`AbstractClassGenerator.create` → `KeyFactory.Generator` →
`CodeEmitter.emit*` → `DebuggingClassWriter.toByteArray` →
`ReflectUtils.defineClass`) is allocate-then-putfield heavy, matching
the RBC.1/SPB.1-9/W2-CHM archetype.

## Why it was never re-verified before being commented out

The original fixture (`apps/cglib_probe`) was never found on this host.
The standalone `net/sf/cglib/` artifact is not one of the 5 target apps
— note Spring's OWN CGLIB usage (via `org/springframework/cglib/`) is a
totally different package and was never covered by this ban in the
first place, so removing CGL.1 has no bearing on Spring's own
`@Configuration` proxying.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `net/sf/cglib/` guard block
(search for `CGL.1`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

Any real, standalone (non-Spring-shaded) CGLIB `Enhancer.create()` usage
under default JIT tiering, watching for a SIGSEGV right after the
generated proxy class's `<clinit>`.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
