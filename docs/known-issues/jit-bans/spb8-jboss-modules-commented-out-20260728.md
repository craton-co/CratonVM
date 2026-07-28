# SPB.8 (JBoss Modules) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — WildFly/JBoss is outside the 5-app scope
(tomcat/hibernate/spring/spring-boot/h2).

## What it banned

`org/jboss/modules/` (blanket package prefix).

## Original symptom (Session 113 r2)

`apps/wildfly-39.0.1.Final` boot SIGSEGV'd (rc=139) right after the
BigInteger `ZERO`/`ONE`/`TWO` post-clinit fixup and two upstream
`<clinit>` swallows (`SimpleLoggerContext`, `ConcurrentClassLoader`).
With `CRATONVM_DISABLE_JIT=1` the SIGSEGV was replaced by a clean
`NoSuchMethodError` for `Object.loadClass(...)` followed by an orderly
`System.exit(1)` — boot proceeded far past the JIT-on crash point,
confirming a JIT miscompile, not a native gap.
`CRATONVM_DBG_JIT_DISPATCH=1` showed the last dispatched method before
the SIGSEGV was `Long.parseLong(String,int)` invoked with a corrupted
reference arg0 (`0xfffd_<heap-ptr>` — a 16-bit tag-corruption signature,
not a valid pointer). Upstream traffic: JBoss Modules's
`PropertyReadAction.run`/`Module$1.run` lambdas iterating module
descriptors, plus JBoss AS's thread-context-classloader doPrivileged
chain — both allocate-then-putfield heavy (`Module.<init>` stores
`name`/`mainClass`/`fallbackLoader`; class-graph traversal allocates
fresh `ResourceLoaderSpec`/`Resource` per visit). Same archetype as
W2-CHM/RBC.1/SPB.1-7.

## Why it was never re-verified before being commented out

WildFly is outside the 5-app scope this session narrowed to. A sibling
ban in this same family (`org/jboss/as/` — SPB.8b) WAS independently
re-verified and removed on 2026-07-27 with a real WildFly boot showing
the class-graph package is now safe; this ban (`org/jboss/modules/`
itself) was not re-tested at that time and was commented out unverified
in this later pass.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `org/jboss/modules/` guard
block (search for `SPB.8` — note SPB.8b, the `org/jboss/as/` companion,
is already permanently removed and documented separately) inside
`should_skip_jit_internal`.

## Repro (for whoever re-verifies)

A real WildFly `standalone.sh`/`domain.sh` boot under default JIT
tiering, with `CRATONVM_DBG_JIT_DISPATCH=1` watching for a corrupted
reference reaching `Long.parseLong` during JBoss Modules class-graph
traversal.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
