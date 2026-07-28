# RBC.1 (BouncyCastle) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — only `tomcat`/`hibernate`/`spring`/`spring-boot`/`h2` need
to work right now; BouncyCastle is a generic crypto provider outside
that scope.

## What it banned

`org/bouncycastle/` (blanket package prefix, with a narrow carveout
function `is_bouncycastle_crypto_hotpath_carveout` for a few methods —
that carveout is now dead code since the broader ban is gone, but was
left in place, harmless).

## Original symptom (Session 109, re-confirmed Round 2 2026-06-03)

`BouncyCastleProvider.<init>` registers ~1000 algorithm mappings in
under a second; many of the mapping classes have hot helper methods the
JIT promotes after a single warm pass. The miscompile is the classic
allocate-then-putfield corruption pattern (same family as
`Integer.valueOf`/`String.toLowerCase`), applied to BC helper objects —
on Windows this manifested as `STATUS_ACCESS_VIOLATION` (rc=139). A
Round-2 investigation separately reproduced an EC-specific crash
(`org/bouncycastle/math/ec/test/AllTests`) under
`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/` and confirmed it is a
distinct upstream value-production miscompile (not the inline-allocation
family that Round 1/2 fixed elsewhere), never root-caused to a specific
producing basic block.

## Why it was never re-verified before being commented out

BouncyCastle is not one of the 5 apps this session narrowed scope to
(it's a generic security-provider library that may be used *by* Tomcat's
TLS stack in some configurations, but the ban itself targets BC
generically, not a Tomcat-specific code path). No fixture was re-run
before commenting it out.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `org/bouncycastle/` guard
block (search for `RBC.1`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

`org/bouncycastle/math/ec/test/AllTests` under
`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`, and a general
`BouncyCastleProvider.<init>` + heavy algorithm use under default JIT
tiering.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
