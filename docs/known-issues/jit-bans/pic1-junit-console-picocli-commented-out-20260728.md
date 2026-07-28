# PIC.1 (JUnit Platform console-standalone picocli) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision.

## What it banned

`org/junit/platform/console/shadow/picocli/` (blanket package prefix) —
the shaded picocli copy JUnit Platform ships inside its
`junit-platform-console-standalone` jar. The **unshaded** picocli copy
(`info/picocli/`) was never covered by this ban and remains
JIT-eligible either way.

## Original symptom (Session 118, 2026-05-25)

`junit-platform-console-standalone-1.10.2.jar -- --help` SEGFAULT'd
(rc=139) right after the BigInteger post-clinit fixup line and before
any picocli help-banner output. `CRATONVM_DBG_JIT_ENTRY=1` showed the
last methods JIT-entered before the crash were
`CommandLine$Model$OptionSpec.equals`, `CommandLine$Assert.equals`,
`CommandLine$Model$ArgSpec.equalsImpl`, and
`CommandLine$Model$CaseAwareLinkedMap.entrySet`/`values`. With
`CRATONVM_JIT_BISECT_ONLY=java/,sun/` (picocli not JIT-eligible) the
SEGFAULT vanished and the run progressed to a clean
`BreakIteratorProviderImpl.getBreakInstance` NPE — a separate,
downstream JDK-locale gap, not a JIT issue. `OptionSpec.equals` is the
canonical allocate-then-putfield archetype (two fresh `HashSet`s
wrapping `Arrays.asList` over each side's `names` array, then
`HashSet.equals`) — same family as W2-CHM/RBC.1/SPB.*.
`CommandLine$Assert.equals`'s dispatch into the JIT-compiled PIC slot
was also independently documented as a **Keycloak-26 SEGFAULT site**
(see `x64.rs:3800` and the SPB.* cascade at the time).

## Why it was never re-verified before being commented out

The JUnit Platform console-standalone launcher tool is not part of any
of the 5 target apps' own test execution path — it's a separate CLI
tool, and its crash is also independently linked to Keycloak (out of
scope). No fixture was re-run before commenting it out.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the
`org/junit/platform/console/shadow/picocli/` guard block (search for
`PIC.1`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

`junit-platform-console-standalone-<version>.jar -- --help` under
default JIT tiering, with `CRATONVM_DBG_JIT_ENTRY=1` watching for a
SIGSEGV in `CommandLine$Model$OptionSpec.equals` or
`CommandLine$Assert.equals`.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
