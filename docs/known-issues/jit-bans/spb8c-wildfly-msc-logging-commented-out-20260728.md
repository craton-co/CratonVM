# SPB.8c (WildFly security manager, MSC, logging) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — WildFly/JBoss is outside the 5-app scope
(tomcat/hibernate/spring/spring-boot/h2).

## What it banned

Three blanket package prefixes:
- `org/wildfly/` (WildFly security manager)
- `org/jboss/msc/` (JBoss Modular Service Container)
- `org/jboss/logging/` (JBoss Logging facade)

## Original symptom (Session 113 r2)

`CRATONVM_DBG_JIT_DISPATCH=1` showed the last JIT dispatches before a
SIGSEGV were `WildFlySecurityManager.<init>` and
`WildFlySecurityManager$2.run`, plus
`GetAccessibleDeclaredFieldAction.run`/`ReadPropertyAction.run`,
immediately followed by an `AccessibleObject.setAccessible0(Z)Z` chain
culminating in `Long.parseLong(String,int)` dispatched with a corrupted
reference arg0 (`0xfffd_<heap-ptr>` — the same 16-bit tag-corruption
signature as SPB.8/`Integer.valueOf`/`String.toLowerCase`). As boot
progressed past `org/wildfly/` (WildFly 39 / session-15), the same
rc=139 SIGSEGV resurfaced in the JBoss MSC service container
(`ServiceName.equals`) and the JBoss Logging facade
(`JDKLogger.<init>`/`LoggerProvider.getLogger`, dispatched ~200x
immediately before the crash) — hence `org/jboss/msc/` and
`org/jboss/logging/` were extended with the same archetype reasoning.

## Why it was never re-verified before being commented out

WildFly is outside the 5-app scope this session narrowed to. A sibling
ban in this same family (`org/jboss/as/` — SPB.8b) WAS independently
re-verified and removed on 2026-07-27 with a real WildFly boot; these
three were not re-tested at that time and were commented out unverified
in this later pass.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `org/wildfly/`,
`org/jboss/msc/`, and `org/jboss/logging/` guard blocks (search for
`SPB.8c`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

A real WildFly boot under default JIT tiering, with
`CRATONVM_DBG_JIT_DISPATCH=1` watching for a corrupted reference
reaching `Long.parseLong` during `WildFlySecurityManager` construction,
MSC `ServiceName.equals`, or JBoss Logging facade initialization.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
